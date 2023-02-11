use crate::core::ServerState;
use crate::engine::handler::following_handler::FollowingHandler;
use crate::engine::handler::log_handler::LogHandler;
use crate::engine::handler::replication_handler::ReplicationHandler;
use crate::engine::handler::server_state_handler::ServerStateHandler;
use crate::engine::handler::snapshot_handler::SnapshotHandler;
use crate::engine::handler::vote_handler::VoteHandler;
use crate::engine::Command;
use crate::entry::RaftEntry;
use crate::error::InitializeError;
use crate::error::NotAMembershipEntry;
use crate::error::NotAllowed;
use crate::error::NotInMembers;
use crate::error::RejectVoteRequest;
use crate::internal_server_state::InternalServerState;
use crate::membership::EffectiveMembership;
use crate::membership::NodeRole;
use crate::node::Node;
use crate::raft::AppendEntriesResponse;
use crate::raft::VoteRequest;
use crate::raft::VoteResponse;
use crate::raft_state::LogStateReader;
use crate::raft_state::RaftState;
use crate::raft_state::VoteStateReader;
use crate::summary::MessageSummary;
use crate::validate::Valid;
use crate::LogId;
use crate::LogIdOptionExt;
use crate::Membership;
use crate::MetricsChangeFlags;
use crate::NodeId;
use crate::SnapshotMeta;
use crate::Vote;

/// Config for Engine
#[derive(Clone, Debug)]
#[derive(PartialEq, Eq)]
pub(crate) struct EngineConfig<NID: NodeId> {
    /// The id of this node.
    pub(crate) id: NID,

    /// The maximum number of applied logs to keep before purging.
    pub(crate) max_in_snapshot_log_to_keep: u64,

    /// The minimal number of applied logs to purge in a batch.
    pub(crate) purge_batch_size: u64,

    /// The maximum number of entries per payload allowed to be transmitted during replication
    pub(crate) max_payload_entries: u64,
}

impl<NID: NodeId> Default for EngineConfig<NID> {
    fn default() -> Self {
        Self {
            id: NID::default(),
            max_in_snapshot_log_to_keep: 1000,
            purge_batch_size: 256,
            max_payload_entries: 300,
        }
    }
}

/// The entry of output from Engine to the runtime.
#[derive(Debug, Clone, Default)]
#[derive(PartialEq, Eq)]
pub(crate) struct EngineOutput<NID, N>
where
    NID: NodeId,
    N: Node,
{
    /// Tracks what kind of metrics changed
    pub(crate) metrics_flags: MetricsChangeFlags,

    /// Command queue that need to be executed by `RaftRuntime`.
    pub(crate) commands: Vec<Command<NID, N>>,
}

impl<NID, N> EngineOutput<NID, N>
where
    NID: NodeId,
    N: Node,
{
    pub(crate) fn push_command(&mut self, cmd: Command<NID, N>) {
        cmd.update_metrics_flags(&mut self.metrics_flags);
        self.commands.push(cmd)
    }
}

/// Raft protocol algorithm.
///
/// It implement the complete raft algorithm except does not actually update any states.
/// But instead, it output commands to let a `RaftRuntime` implementation execute them to actually
/// update the states such as append-log or save-vote by execute .
///
/// This structure only contains necessary information to run raft algorithm,
/// but none of the application specific data.
/// TODO: make the fields private
#[derive(Debug, Clone, Default)]
#[derive(PartialEq, Eq)]
pub(crate) struct Engine<NID, N>
where
    NID: NodeId,
    N: Node,
{
    pub(crate) config: EngineConfig<NID>,

    /// The state of this raft node.
    pub(crate) state: Valid<RaftState<NID, N>>,

    /// The internal server state used by Engine.
    pub(crate) internal_server_state: InternalServerState<NID>,

    /// Output entry for the runtime.
    pub(crate) output: EngineOutput<NID, N>,
}

impl<NID, N> Engine<NID, N>
where
    N: Node,
    NID: NodeId,
{
    pub(crate) fn new(init_state: RaftState<NID, N>, config: EngineConfig<NID>) -> Self {
        Self {
            config,
            state: Valid::new(init_state),
            internal_server_state: InternalServerState::default(),
            output: EngineOutput::default(),
        }
    }

    // TODO: test it
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn startup(&mut self) {
        // Allows starting up as a leader.

        // Previously it is a leader. restore it as leader at once
        if self.state.is_leader(&self.config.id) {
            self.vote_handler().update_internal_server_state();

            let mut rh = self.replication_handler();
            rh.rebuild_replication_streams();
            rh.initiate_replication();

            return;
        }

        let server_state = if self.state.membership_state.effective().is_voter(&self.config.id) {
            ServerState::Follower
        } else {
            ServerState::Learner
        };

        self.state.server_state = server_state;

        tracing::debug!(
            "startup: id={} target_state: {:?}",
            self.config.id,
            self.state.server_state
        );
    }

    /// Initialize a node by appending the first log.
    ///
    /// - The first log has to be membership config log.
    /// - The node has to contain no logs at all and the vote is the minimal value. See: [Conditions
    ///   for initialization](https://datafuselabs.github.io/openraft/cluster-formation.html#conditions-for-initialization)
    ///
    /// Appending the very first log is slightly different from appending log by a leader or
    /// follower. This step is not confined by the consensus protocol and has to be dealt with
    /// differently.
    #[tracing::instrument(level = "debug", skip(self, entries))]
    pub(crate) fn initialize<Ent: RaftEntry<NID, N>>(
        &mut self,
        entries: &mut [Ent],
    ) -> Result<(), InitializeError<NID, N>> {
        let l = entries.len();
        debug_assert_eq!(1, l);

        self.check_initialize()?;

        self.assign_log_ids(entries.iter_mut());
        self.state.extend_log_ids_from_same_leader(entries);

        self.output.push_command(Command::AppendInputEntries { range: 0..l });

        let entry = &mut entries[0];
        if let Some(m) = entry.get_membership() {
            self.check_members_contain_me(m)?;
        } else {
            Err(NotAMembershipEntry {})?;
        }

        if let Some(m) = entry.get_membership() {
            let log_id = entry.get_log_id();
            tracing::debug!("update effective membership: log_id:{} {}", log_id, m.summary());

            let em = EffectiveMembership::new_arc(Some(*log_id), m.clone());
            self.state.membership_state.append(em.clone());
            self.output.push_command(Command::UpdateMembership { membership: em });
            self.server_state_handler().update_server_state_if_changed();
        }

        self.output.push_command(Command::MoveInputCursorBy { n: l });

        // With the new config, start to elect to become leader
        self.elect();

        Ok(())
    }

    /// Start to elect this node as leader
    #[tracing::instrument(level = "debug", skip(self))]
    pub(crate) fn elect(&mut self) {
        let v = Vote::new(self.state.get_vote().term + 1, self.config.id);
        self.vote_handler().handle_message_vote(&v).unwrap();

        // Safe unwrap()
        let leader = self.internal_server_state.leading_mut().unwrap();
        leader.grant_vote_by(self.config.id);
        let quorum_granted = leader.is_vote_granted();

        // Fast-path: if there is only one node in the cluster.

        if quorum_granted {
            self.establish_leader();
            return;
        }

        // Slow-path: send vote request, let a quorum grant it.

        self.output.push_command(Command::SendVote {
            vote_req: VoteRequest::new(*self.state.get_vote(), self.state.last_log_id().copied()),
        });

        // TODO: For compatibility. remove it. The runtime does not need to know about server state.
        self.server_state_handler().update_server_state_if_changed();
        self.output.push_command(Command::InstallElectionTimer { can_be_leader: true });
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn handle_vote_req(&mut self, req: VoteRequest<NID>) -> VoteResponse<NID> {
        tracing::debug!(req = display(req.summary()), "Engine::handle_vote_req");
        tracing::debug!(
            my_vote = display(self.state.get_vote().summary()),
            my_last_log_id = display(self.state.last_log_id().summary()),
            "Engine::handle_vote_req"
        );

        // TODO: refactor
        let res = if req.last_log_id.as_ref() >= self.state.last_log_id() {
            self.vote_handler().handle_message_vote(&req.vote)
        } else {
            Err(RejectVoteRequest::ByLastLogId(self.state.last_log_id().copied()))
        };

        let vote_granted = if let Err(reject) = res {
            tracing::debug!(
                req = display(req.summary()),
                err = display(reject),
                "reject vote request"
            );
            false
        } else {
            true
        };

        VoteResponse {
            // Return the updated vote, this way the candidate knows which vote is granted, in case
            // the candidate's vote is changed after sending the vote request.
            vote: *self.state.get_vote(),
            vote_granted,
            last_log_id: self.state.last_log_id().copied(),
        }
    }

    #[tracing::instrument(level = "debug", skip(self, resp))]
    pub(crate) fn handle_vote_resp(&mut self, target: NID, resp: VoteResponse<NID>) {
        tracing::debug!(
            resp = display(resp.summary()),
            target = display(target),
            "handle_vote_resp"
        );
        tracing::debug!(
            my_vote = display(self.state.get_vote()),
            my_last_log_id = display(self.state.last_log_id().summary()),
            "handle_vote_resp"
        );

        // If this node is no longer a leader(i.e., electing), just ignore the delayed vote_resp.
        let leader = match &mut self.internal_server_state {
            InternalServerState::Leading(l) => l,
            InternalServerState::Following => return,
        };

        if &resp.vote < self.state.get_vote() {
            debug_assert!(!resp.vote_granted);
        }

        if resp.vote_granted {
            leader.grant_vote_by(target);

            let quorum_granted = leader.is_vote_granted();
            if quorum_granted {
                tracing::debug!("quorum granted vote");
                self.establish_leader();
            }
            return;
        }

        // vote is rejected:

        debug_assert_eq!(
            Some(NodeRole::Voter),
            self.state.membership_state.effective().get_node_role(&self.config.id)
        );

        // If peer's vote is greater than current vote, revert to follower state.
        if &resp.vote > self.state.get_vote() {
            self.state.vote = resp.vote;
            self.output.push_command(Command::SaveVote {
                vote: *self.state.get_vote(),
            });
        }

        // Seen a higher log.
        // TODO: if already installed a timer with can_be_leader==false, it should not install a
        // timer with       can_be_leader==true.
        if resp.last_log_id.as_ref() > self.state.last_log_id() {
            self.output.push_command(Command::InstallElectionTimer { can_be_leader: false });
        } else {
            self.output.push_command(Command::InstallElectionTimer { can_be_leader: true });
        }

        debug_assert!(self.state.is_voter(&self.config.id));

        // When vote is rejected, it does not need to leave candidate state.
        // Candidate loop, follower loop and learner loop are totally the same.
        //
        // The only thing that needs to do is update election timer.
    }

    /// Append new log entries by a leader.
    ///
    /// Also Update effective membership if the payload contains
    /// membership config.
    ///
    /// If there is a membership config log entry, the caller has to guarantee the previous one is
    /// committed.
    ///
    /// TODO(xp): metrics flag needs to be dealt with.
    /// TODO(xp): if vote indicates this node is not the leader, refuse append
    #[tracing::instrument(level = "debug", skip(self, entries))]
    pub(crate) fn leader_append_entries<'a, Ent: RaftEntry<NID, N> + 'a>(&mut self, entries: &mut [Ent]) {
        let l = entries.len();
        if l == 0 {
            return;
        }

        self.assign_log_ids(entries.iter_mut());
        self.state.extend_log_ids_from_same_leader(entries);

        self.output.push_command(Command::AppendInputEntries { range: 0..l });

        // Fast commit:
        // If the cluster has only one voter, then an entry will be committed as soon as it is
        // appended. But if there is a membership log in the middle of the input entries,
        // the condition to commit will change. Thus we have to deal with entries before and
        // after a membership entry differently:
        //
        // When a membership entry is seen, update progress for all former entries.
        // Then upgrade the quorum set for the Progress.
        //
        // E.g., if the input entries are `2..6`, entry 4 changes membership from `a` to `abc`.
        // Then it will output a LeaderCommit command to commit entries `2,3`.
        // ```text
        // 1 2 3 4 5 6
        // a x x a y y
        //       b
        //       c
        // ```
        //
        // If the input entries are `2..6`, entry 4 changes membership from `abc` to `a`.
        // Then it will output a LeaderCommit command to commit entries `2,3,4,5,6`.
        // ```text
        // 1 2 3 4 5 6
        // a x x a y y
        // b
        // c
        // ```

        let mut rh = self.replication_handler();

        for entry in entries.iter() {
            if let Some(m) = entry.get_membership() {
                let log_index = entry.get_log_id().index;

                if log_index > 0 {
                    let prev_log_id = rh.state.get_log_id(log_index - 1);
                    rh.update_local_progress(prev_log_id);
                }

                // since this entry, the condition to commit has been changed.
                rh.append_membership(entry.get_log_id(), m);
            }
        }

        let last_log_id = {
            // Safe unwrap(): entries.len() > 0
            let last = entries.last().unwrap();
            Some(*last.get_log_id())
        };

        rh.update_local_progress(last_log_id);
        rh.initiate_replication();

        self.output.push_command(Command::MoveInputCursorBy { n: l });
    }

    // TODO: move logic to FollowingHandler
    /// Append entries to follower/learner.
    ///
    /// Also clean conflicting entries and update membership state.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn handle_append_entries_req<'a, Ent>(
        &mut self,
        vote: &Vote<NID>,
        prev_log_id: Option<LogId<NID>>,
        entries: &[Ent],
        leader_committed: Option<LogId<NID>>,
    ) -> AppendEntriesResponse<NID>
    where
        Ent: RaftEntry<NID, N> + MessageSummary<Ent> + 'a,
    {
        tracing::debug!(
            vote = display(vote),
            prev_log_id = display(prev_log_id.summary()),
            entries = display(entries.summary()),
            leader_committed = display(leader_committed.summary()),
            "append-entries request"
        );
        tracing::debug!(
            my_vote = display(self.state.get_vote()),
            my_last_log_id = display(self.state.last_log_id().summary()),
            my_committed = display(self.state.committed().summary()),
            "local state"
        );

        let res = self.vote_handler().handle_message_vote(vote);
        if let Err(rejected) = res {
            return rejected.into();
        }

        // Vote is legal. Check if prev_log_id matches local raft-log.

        if let Some(ref prev) = prev_log_id {
            if !self.state.has_log_id(prev) {
                let local = self.state.get_log_id(prev.index);
                tracing::debug!(local = debug(&local), "prev_log_id does not match");

                self.following_handler().truncate_logs(prev.index);
                return AppendEntriesResponse::Conflict;
            }
        }
        // else `prev_log_id.is_none()` means replicating logs from the very beginning.

        tracing::debug!(
            committed = display(self.state.committed().summary()),
            entries = display(entries.summary()),
            "prev_log_id matches, skip matching entries",
        );

        let l = entries.len();
        let since = self.following_handler().state.first_conflicting_index(entries);
        if since < l {
            // Before appending, if an entry overrides an conflicting one,
            // the entries after it has to be deleted first.
            // Raft requires log ids are in total order by (term,index).
            // Otherwise the log id with max index makes committed entry invisible in election.
            self.following_handler().truncate_logs(entries[since].get_log_id().index);
            self.following_handler().follower_do_append_entries(entries, since);
        }

        self.following_handler().follower_commit_entries(leader_committed, prev_log_id, entries);

        AppendEntriesResponse::Success
    }

    /// Leader steps down(convert to learner) once the membership not containing it is committed.
    ///
    /// This is only called by leader.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn leader_step_down(&mut self) {
        tracing::debug!("leader_step_down: node_id:{}", self.config.id);

        // Step down:
        // Keep acting as leader until a membership without this node is committed.
        let em = &self.state.membership_state.effective();

        tracing::debug!(
            "membership: {}, committed: {}, is_leading: {}",
            em.summary(),
            self.state.committed().summary(),
            self.state.is_leading(&self.config.id),
        );

        #[allow(clippy::collapsible_if)]
        if em.log_id.as_ref() <= self.state.committed() {
            if !em.is_voter(&self.config.id) && self.state.is_leading(&self.config.id) {
                tracing::debug!("leader {} is stepping down", self.config.id);
                self.vote_handler().become_following();
            }
        }
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn finish_building_snapshot(&mut self, meta: SnapshotMeta<NID, N>) {
        tracing::info!("finish_building_snapshot: {:?}", meta);

        let mut h = self.snapshot_handler();

        let updated = h.update_snapshot(meta);
        if !updated {
            return;
        }

        self.log_handler().update_purge_upto();

        if self.internal_server_state.is_leading() {
            // If it is leading, it must not delete a log that is in use by a replication task.
            self.replication_handler().try_purge_log();
        } else {
            // For follower/learner, no other tasks are using logs, just purge.
            self.log_handler().purge_log();
        }
    }
}

/// Supporting util
impl<NID, N> Engine<NID, N>
where
    N: Node,
    NID: NodeId,
{
    /// Vote is granted by a quorum, leader established.
    #[tracing::instrument(level = "debug", skip_all)]
    fn establish_leader(&mut self) {
        self.vote_handler().commit_vote();

        let mut rh = self.replication_handler();

        // It has to setup replication stream first because append_blank_log() may update the
        // committed-log-id(a single leader with several learners), in which case the
        // committed-log-id will be at once submitted to replicate before replication stream
        // is built. TODO: But replication streams should be built when a node enters
        // leading state.       Thus append_blank_log() can be moved before
        // rebuild_replication_streams()

        rh.rebuild_replication_streams();
        rh.append_blank_log();
        rh.initiate_replication();
    }

    /// Check if a raft node is in a state that allows to initialize.
    ///
    /// It is allowed to initialize only when `last_log_id.is_none()` and `vote==(term=0,
    /// node_id=0)`. See: [Conditions for initialization](https://datafuselabs.github.io/openraft/cluster-formation.html#conditions-for-initialization)
    fn check_initialize(&self) -> Result<(), NotAllowed<NID>> {
        if self.state.last_log_id().is_none() && self.state.get_vote() == &Vote::default() {
            return Ok(());
        }

        tracing::error!(
            last_log_id = display(self.state.last_log_id().summary()),
            vote = display(self.state.get_vote()),
            "Can not initialize"
        );

        Err(NotAllowed {
            last_log_id: self.state.last_log_id().copied(),
            vote: *self.state.get_vote(),
        })
    }

    /// When initialize, the node that accept initialize request has to be a member of the initial
    /// config.
    fn check_members_contain_me(&self, m: &Membership<NID, N>) -> Result<(), NotInMembers<NID, N>> {
        if !m.is_voter(&self.config.id) {
            let e = NotInMembers {
                node_id: self.config.id,
                membership: m.clone(),
            };
            Err(e)
        } else {
            Ok(())
        }
    }

    fn assign_log_ids<'a, Ent: RaftEntry<NID, N> + 'a>(&mut self, entries: impl Iterator<Item = &'a mut Ent>) {
        let mut log_id = LogId::new(self.state.get_vote().leader_id(), self.state.last_log_id().next_index());
        for entry in entries {
            entry.set_log_id(&log_id);
            tracing::debug!("assign log id: {}", log_id);
            log_id.index += 1;
        }
    }

    // Only used by tests
    #[allow(dead_code)]
    pub(crate) fn calc_server_state(&self) -> ServerState {
        self.state.calc_server_state(&self.config.id)
    }

    // --- handlers ---

    pub(crate) fn vote_handler(&mut self) -> VoteHandler<NID, N> {
        VoteHandler {
            config: &self.config,
            state: &mut self.state,
            output: &mut self.output,
            internal_server_state: &mut self.internal_server_state,
        }
    }

    pub(crate) fn log_handler(&mut self) -> LogHandler<NID, N> {
        LogHandler {
            config: &mut self.config,
            state: &mut self.state,
            output: &mut self.output,
        }
    }

    pub(crate) fn snapshot_handler(&mut self) -> SnapshotHandler<NID, N> {
        SnapshotHandler {
            state: &mut self.state,
            output: &mut self.output,
        }
    }

    pub(crate) fn replication_handler(&mut self) -> ReplicationHandler<NID, N> {
        let leader = match self.internal_server_state.leading_mut() {
            None => {
                unreachable!("There is no leader, can not handle replication");
            }
            Some(x) => x,
        };

        ReplicationHandler {
            config: &mut self.config,
            leader,
            state: &mut self.state,
            output: &mut self.output,
        }
    }

    pub(crate) fn following_handler(&mut self) -> FollowingHandler<NID, N> {
        debug_assert!(self.internal_server_state.is_following());

        FollowingHandler {
            config: &mut self.config,
            state: &mut self.state,
            output: &mut self.output,
        }
    }

    pub(crate) fn server_state_handler(&mut self) -> ServerStateHandler<NID, N> {
        ServerStateHandler {
            config: &self.config,
            state: &mut self.state,
            output: &mut self.output,
        }
    }
}
