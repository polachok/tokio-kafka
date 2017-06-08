use std::mem;
use std::rc::Rc;
use std::cell::RefCell;
use std::time::{Duration, Instant};
use std::iter::FromIterator;
use std::collections::{HashMap, HashSet};

use futures::{Future, Stream};
use tokio_timer::Timer;

use errors::{Error, ErrorKind, Result};
use protocol::{KafkaCode, Schema, ToMilliseconds};
use client::{BrokerRef, Client, ConsumerGroupAssignment, ConsumerGroupMember,
             ConsumerGroupProtocol, Generation, KafkaClient, Metadata, StaticBoxFuture};
use consumer::{Assignment, CONSUMER_PROTOCOL, PartitionAssignor, Subscription, Subscriptions};

/// Manages the coordination process with the consumer coordinator.
pub trait Coordinator {
    /// Join the consumer group.
    fn join_group(&mut self) -> JoinGroup;

    /// Leave the current consumer group.
    fn leave_group(&mut self) -> LeaveGroup;
}

pub type JoinGroup = StaticBoxFuture;

pub type LeaveGroup = StaticBoxFuture;

/// Manages the coordination process with the consumer coordinator.
pub struct ConsumerCoordinator<'a> {
    inner: Rc<Inner<'a>>,
}

struct Inner<'a> {
    client: KafkaClient<'a>,
    group_id: String,
    subscriptions: RefCell<Subscriptions<'a>>,
    session_timeout: Duration,
    rebalance_timeout: Duration,
    heartbeat_interval: Duration,
    retry_backoff: Duration,
    assignors: Vec<Box<PartitionAssignor>>,
    state: Rc<RefCell<State>>,
    timer: Rc<Timer>,
}

enum State {
    /// the client is not part of a group
    Unjoined,
    /// the client has begun rebalancing
    Rebalancing,
    /// the client has joined and is sending heartbeats
    Stable {
        coordinator: BrokerRef,
        generation: Generation,
    },
}

impl State {
    pub fn member_id(&self) -> Option<String> {
        if let State::Stable { ref generation, .. } = *self {
            Some(String::from(generation.member_id.to_owned()))
        } else {
            None
        }
    }

    pub fn rebalance(&mut self) -> Self {
        mem::replace(self, State::Rebalancing)
    }

    pub fn joined(&mut self, coordinator: BrokerRef, generation: Generation) -> State {
        mem::replace(self,
                     State::Stable {
                         coordinator: coordinator,
                         generation: generation,
                     })
    }

    pub fn leave(&mut self) -> Self {
        mem::replace(self, State::Unjoined)
    }
}

impl<'a> ConsumerCoordinator<'a> {
    pub fn new(client: KafkaClient<'a>,
               group_id: String,
               subscriptions: Subscriptions<'a>,
               session_timeout: Duration,
               rebalance_timeout: Duration,
               heartbeat_interval: Duration,
               retry_backoff: Duration,
               assignors: Vec<Box<PartitionAssignor>>,
               timer: Rc<Timer>)
               -> Self {
        ConsumerCoordinator {
            inner: Rc::new(Inner {
                               client: client,
                               group_id: group_id,
                               subscriptions: RefCell::new(subscriptions),
                               session_timeout: session_timeout,
                               rebalance_timeout: rebalance_timeout,
                               heartbeat_interval: heartbeat_interval,
                               retry_backoff: retry_backoff,
                               assignors: assignors,
                               timer: timer,
                               state: Rc::new(RefCell::new(State::Unjoined)),
                           }),
        }
    }
}

impl<'a> Inner<'a>
    where Self: 'static
{
    fn group_protocols(&self) -> Vec<ConsumerGroupProtocol<'a>> {
        let topics: Vec<String> = self.subscriptions
            .borrow()
            .topics()
            .iter()
            .map(|topic_name| String::from(*topic_name))
            .collect();

        self.assignors
            .iter()
            .flat_map(move |assignor| {
                let subscription =
                    assignor.subscription(topics
                                              .iter()
                                              .map(|topic_name| topic_name.as_str().into())
                                              .collect());

                Schema::serialize(&subscription)
                    .map_err(|err| warn!("fail to serialize subscription, {}", err))
                    .ok()
                    .map(|metadata| {
                             ConsumerGroupProtocol {
                                 protocol_name: assignor.name().into(),
                                 protocol_metadata: metadata.into(),
                             }
                         })
            })
            .collect()
    }

    fn perform_assignment(&self,
                          metadata: &Metadata,
                          group_protocol: &str,
                          members: &[ConsumerGroupMember])
                          -> Result<Vec<ConsumerGroupAssignment<'a>>> {
        let strategy = group_protocol.parse()?;
        let assignor = self.assignors
            .iter()
            .find(|assigner| assigner.strategy() == strategy)
            .ok_or_else(|| ErrorKind::UnsupportedAssignmentStrategy(group_protocol.to_owned()))?;

        let mut subscripbed_topics = HashSet::new();
        let mut subscriptions = HashMap::new();

        for member in members {
            let subscription: Subscription = Schema::deserialize(member.member_metadata.as_ref())?;

            subscripbed_topics.extend(subscription.topics.iter().cloned());
            subscriptions.insert(member.member_id.as_str().into(), subscription);
        }

        let assignment = assignor.assign(metadata, subscriptions);

        // user-customized assignor may have created some topics that are not in the subscription
        // list and assign their partitions to the members; in this case we would like to update the
        // leader's own metadata with the newly added topics so that it will not trigger a
        // subsequent rebalance when these topics gets updated from metadata refresh.

        let mut assigned_topics = HashSet::new();

        assigned_topics.extend(assignment
                                   .values()
                                   .flat_map(|member| {
                                                 member.partitions.iter().map(|tp| {
                                                                                  tp.topic_name
                                                                                      .clone()
                                                                              })
                                             }));

        let not_assigned_topics = &subscripbed_topics - &assigned_topics;

        if !not_assigned_topics.is_empty() {
            warn!("The following subscribed topics are not assigned to any members in the group `{}`: {}",
                  self.group_id,
                  Vec::from_iter(not_assigned_topics.iter().cloned())
                      .as_slice()
                      .join(","));
        }

        let newly_added_topics = &assigned_topics - &subscripbed_topics;

        if !newly_added_topics.is_empty() {
            info!("The following not-subscribed topics are assigned to group {}, and their metadata will be fetched from the brokers : {}",
                  self.group_id,
                  Vec::from_iter(newly_added_topics.iter().cloned())
                      .as_slice()
                      .join(","));

            subscripbed_topics.extend(assigned_topics);
        }

        self.subscriptions
            .borrow_mut()
            .group_subscribe(subscripbed_topics.iter());

        let mut group_assignment = Vec::new();

        for (member_id, assignment) in assignment {
            group_assignment.push(ConsumerGroupAssignment {
                                      member_id: String::from(member_id).into(),
                                      member_assignment: Schema::serialize(&assignment)?.into(),
                                  })
        }

        Ok(group_assignment)
    }

    fn synced_group(&self,
                    assignment: Assignment<'a>,
                    coordinator: BrokerRef,
                    generation: Generation)
                    -> Result<()> {
        trace!("member `{}` synced up to generation # {} with {} partitions: {:?}",
               generation.member_id,
               generation.generation_id,
               assignment.partitions.len(),
               assignment.partitions);

        self.subscriptions
            .borrow_mut()
            .assign_from_subscribed(assignment.partitions)?;

        self.state
            .borrow_mut()
            .joined(coordinator, generation.clone());

        let client = self.client.clone();

        self.client
            .handle()
            .spawn(self.timer
                       .interval_at(Instant::now() + self.heartbeat_interval,
                                    self.heartbeat_interval)
                       .map_err(Error::from)
                       .for_each(move |_| client.heartbeat(coordinator, generation.clone()))
                       .map_err(|err| {
                                    warn!("fail to send heartbeat, {}", err);
                                }));

        Ok(())
    }
}

impl<'a> Coordinator for ConsumerCoordinator<'a>
    where Self: 'static
{
    fn join_group(&mut self) -> JoinGroup {
        self.inner.state.borrow_mut().rebalance();

        let inner = self.inner.clone();
        let client = self.inner.client.clone();
        let member_id = self.inner.state.borrow().member_id().unwrap_or_default();
        let group_id = self.inner.group_id.clone();
        let session_timeout = self.inner.session_timeout;
        let rebalance_timeout = self.inner.rebalance_timeout;
        let group_protocols = self.inner.group_protocols();
        let state = self.inner.state.clone();

        debug!("member `{}` is joining the `{}` group", member_id, group_id);

        let future = self.inner
            .client
            .metadata()
            .join(self.inner.client.group_coordinator(group_id.clone().into()))
            .and_then(move |(metadata, coordinator)| {
                client
                    .join_group(coordinator.as_ref(),
                                group_id.clone().into(),
                                session_timeout.as_millis() as i32,
                                rebalance_timeout.as_millis() as i32,
                                member_id.clone().into(),
                                CONSUMER_PROTOCOL.into(),
                                group_protocols)
                    .and_then(move |consumer_group| {
                        let generation = consumer_group.generation();

                        let group_assignment = if !consumer_group.is_leader() {
                            debug!("member `{}` joined group `{}` as follower",
                                   member_id,
                                   group_id);

                            None
                        } else {
                            debug!("member `{}` joined group `{}` as leader",
                                   member_id,
                                   group_id);

                            match inner.perform_assignment(&metadata,
                                                           &consumer_group.protocol,
                                                           &consumer_group.members) {
                                Ok(group_assignment) => Some(group_assignment),
                                Err(err) => return JoinGroup::err(err),
                            }
                        };

                        let future = client
                            .sync_group(coordinator.as_ref(), generation.clone(), group_assignment)
                            .and_then(move |assignment| {
                                          debug!("group `{}` synced up", group_id);

                                          inner.synced_group(Schema::deserialize(&assignment[..])?,
                                                             coordinator.as_ref(),
                                                             generation)
                                      });

                        JoinGroup::new(future)
                    })
            })
            .map_err(move |err| {
                         warn!("fail to join group, {}", err);

                         state.borrow_mut().leave();

                         err
                     });

        JoinGroup::new(future)
    }

    fn leave_group(&mut self) -> LeaveGroup {
        let state = self.inner.state.borrow_mut().leave();

        if let State::Stable {
                   coordinator,
                   generation,
               } = state {
            let group_id = self.inner.group_id.clone();

            debug!("member `{}` is leaving the `{}` group",
                   generation.member_id,
                   group_id);

            LeaveGroup::new(self.inner
                                .client
                                .leave_group(coordinator, generation)
                                .map(|group_id| {
                                         debug!("member has leaved the `{}` group", group_id);
                                     }))
        } else {
            LeaveGroup::err(ErrorKind::KafkaError(KafkaCode::GroupLoadInProgress).into())
        }
    }
}
