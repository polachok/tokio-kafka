use std::mem;
use std::rc::Rc;
use std::borrow::Cow;
use std::fmt::Debug;
use std::cell::RefCell;
use std::ops::Deref;
use std::net::{SocketAddr, ToSocketAddrs};
use std::collections::HashMap;
use std::iter::FromIterator;
use std::time::Duration;

use bytes::Bytes;

use rand::{self, Rng};

use futures::{Async, IntoFuture, Poll};
use futures::future::{self, Future};
use futures::unsync::oneshot;
use tokio_core::reactor::{Handle, Timeout};
use tokio_service::Service;
use tokio_middleware::{Log as LogMiddleware, Timeout as TimeoutMiddleware};
use tokio_timer::Timer;

use errors::{Error, ErrorKind, Result};
use protocol::{ApiKeys, ApiVersion, CorrelationId, ErrorCode, FetchOffset, GenerationId,
               JoinGroupMember, JoinGroupProtocol, KafkaCode, MessageSet, Offset, PartitionId,
               RequiredAcks, SyncGroupAssignment, UsableApiVersions};
use network::{KafkaRequest, KafkaResponse, TopicPartition};
use client::{Broker, BrokerRef, ClientBuilder, ClientConfig, Cluster, KafkaService, Metadata,
             Metrics};

/// A trait for communicating with the Kafka cluster.
pub trait Client<'a>: 'static {
    /// Send the given record asynchronously and return a future which will eventually contain the response information.
    fn produce_records(&self,
                       acks: RequiredAcks,
                       timeout: Duration,
                       topic_partition: TopicPartition<'a>,
                       records: Vec<Cow<'a, MessageSet>>)
                       -> ProduceRecords;

    /// Search the offsets by target times for the specified topics and return a future which will eventually contain the partition offset information.
    fn fetch_offsets(&self,
                     partitions: Vec<TopicPartition<'a>>,
                     offset: FetchOffset)
                     -> FetchOffsets;

    /// Load metadata of the Kafka cluster and return a future which will eventually contain the metadata information.
    fn load_metadata(&mut self) -> LoadMetadata<'a>;

    /// Discover the current coordinator of the consumer group.
    fn group_coordinator(&self, group_id: Cow<'a, str>) -> GroupCoordinator;

    /// Join the consumer group
    fn join_group(&self,
                  coordinator: BrokerRef,
                  group_id: Cow<'a, str>,
                  session_timeout: i32,
                  rebalance_timeout: i32,
                  member_id: Cow<'a, str>,
                  protocol_type: Cow<'a, str>,
                  group_protocols: Vec<ConsumerGroupProtocol<'a>>)
                  -> JoinGroup;

    /// Send heartbeat to the consumer group
    fn heartbeat(&self, coordinator: BrokerRef, generation: Generation) -> Heartbeat;

    /// Leave the current consumer group
    fn leave_group(&self, coordinator: BrokerRef, generation: Generation) -> LeaveGroup;

    /// Sync the current consumer group
    fn sync_group(&self,
                  coordinator: BrokerRef,
                  generation: Generation,
                  group_assignment: Option<Vec<ConsumerGroupAssignment<'a>>>)
                  -> SyncGroup;
}

/// The future of records metadata information.
pub type ProduceRecords = StaticBoxFuture<HashMap<String, Vec<(PartitionId, ErrorCode, Offset)>>>;

/// The future of partition offsets information.
pub type FetchOffsets = StaticBoxFuture<HashMap<String, Vec<PartitionOffset>>>;

/// The partition and offset
#[derive(Clone, Debug)]
pub struct PartitionOffset {
    /// The partition id
    pub partition: PartitionId,
    /// The offset found in the partition
    pub offset: Offset,
}

/// The future of discover group coodinator
pub type GroupCoordinator = StaticBoxFuture<Broker>;

/// The future of join group.
pub type JoinGroup = StaticBoxFuture<ConsumerGroup>;

pub type ConsumerGroupProtocol<'a> = JoinGroupProtocol<'a>;

/// The future of heartbeat.
pub type Heartbeat = StaticBoxFuture;

/// The consumer group
pub struct ConsumerGroup {
    /// The group id.
    pub group_id: String,

    /// The generation of the consumer group.
    pub generation_id: GenerationId,

    /// The group protocol selected by the coordinator
    pub protocol: String,

    /// The leader of the group
    pub leader_id: String,

    /// The consumer id assigned by the group coordinator.
    pub member_id: String,

    /// The members of the group
    pub members: Vec<ConsumerGroupMember>,
}

impl ConsumerGroup {
    pub fn is_leader(&self) -> bool {
        self.leader_id == self.member_id
    }

    pub fn generation(&self) -> Generation {
        Generation {
            group_id: self.group_id.clone(),
            generation_id: self.generation_id,
            member_id: self.member_id.clone(),
            protocol: self.protocol.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Generation {
    /// The group id.
    pub group_id: String,

    /// The generation of the consumer group.
    pub generation_id: GenerationId,

    /// The consumer id assigned by the group coordinator.
    pub member_id: String,

    /// The group protocol selected by the coordinator
    pub protocol: String,
}

/// The consumer group member
pub type ConsumerGroupMember = JoinGroupMember;

/// The future of leave consumer group.
pub type LeaveGroup = StaticBoxFuture<String>;

pub type ConsumerGroupAssignment<'a> = SyncGroupAssignment<'a>;

/// The future of sync consumer group.
pub type SyncGroup = StaticBoxFuture<Bytes>;

/// A Kafka client that communicate with the Kafka cluster.
#[derive(Clone)]
pub struct KafkaClient<'a> {
    inner: Rc<Inner<'a>>,
}

struct Inner<'a> {
    config: ClientConfig,
    handle: Handle,
    service: InFlightMiddleware<LogMiddleware<TimeoutMiddleware<KafkaService<'a>>>>,
    timer: Rc<Timer>,
    metrics: Option<Rc<Metrics>>,
    state: Rc<RefCell<State>>,
}

#[derive(Default)]
struct State {
    correlation_id: CorrelationId,
    metadata_status: MetadataStatus,
}

enum MetadataStatus {
    Loading(RefCell<Vec<oneshot::Sender<Rc<Metadata>>>>),
    Loaded(Rc<Metadata>),
}

impl Default for MetadataStatus {
    fn default() -> Self {
        MetadataStatus::Loading(RefCell::new(Vec::new()))
    }
}

impl<'a> Deref for KafkaClient<'a> {
    type Target = ClientConfig;

    fn deref(&self) -> &Self::Target {
        &self.inner.config
    }
}

impl<'a> KafkaClient<'a>
    where Self: 'static
{
    pub fn from_hosts<I>(hosts: I, handle: Handle) -> ClientBuilder<'a>
        where I: Iterator<Item = SocketAddr>
    {
        ClientBuilder::from_hosts(hosts, handle)
    }

    pub fn from_config(config: ClientConfig, handle: Handle) -> KafkaClient<'a> {
        trace!("create client from config: {:?}", config);

        let metrics = if config.metrics {
            Some(Rc::new(Metrics::new().expect("fail to register metrics")))
        } else {
            None
        };
        let service = InFlightMiddleware::new(
            LogMiddleware::new(
                TimeoutMiddleware::new(
                    KafkaService::new(handle.clone(),
                                      config.max_connection_idle(),
                                      metrics.clone()),
                                      config.timer(),
                                      config.request_timeout())));

        let timer = Rc::new(config.timer());
        let inner = Rc::new(Inner {
                                config: config,
                                handle: handle.clone(),
                                service: service,
                                timer: timer,
                                metrics: metrics,
                                state: Rc::new(RefCell::new(State::default())),
                            });

        let mut client = KafkaClient { inner: inner };

        client.refresh_metadata();

        client
    }

    pub fn handle(&self) -> &Handle {
        &self.inner.handle
    }

    pub fn timer(&self) -> Rc<Timer> {
        self.inner.timer.clone()
    }

    pub fn metrics(&self) -> Option<Rc<Metrics>> {
        self.inner.metrics.clone()
    }

    pub fn metadata(&self) -> GetMetadata {
        (*self.inner.state).borrow().metadata()
    }

    pub fn refresh_metadata(&mut self) {
        let handle = self.inner.handle.clone();

        handle.spawn(self.load_metadata()
                         .map(|metadata| {
                                  trace!("auto loaded metadata, {:?}", metadata);
                              })
                         .map_err(|err| {
                                      warn!("fail to load metadata, {}", err);
                                  }));
    }
}

impl<'a> Client<'a> for KafkaClient<'a>
    where Self: 'static
{
    fn produce_records(&self,
                       required_acks: RequiredAcks,
                       timeout: Duration,
                       tp: TopicPartition<'a>,
                       records: Vec<Cow<'a, MessageSet>>)
                       -> ProduceRecords {
        let inner = self.inner.clone();
        let future = self.metadata()
            .and_then(move |metadata| {
                          inner.produce_records(metadata, required_acks, timeout, tp, records)
                      });
        ProduceRecords::new(future)
    }

    fn fetch_offsets(&self,
                     partitions: Vec<TopicPartition<'a>>,
                     offset: FetchOffset)
                     -> FetchOffsets {
        let inner = self.inner.clone();
        let future = self.metadata()
            .and_then(move |metadata| {
                          let topics = inner.topics_by_broker(metadata, partitions);

                          inner.fetch_offsets(topics, offset)
                      });
        FetchOffsets::new(future)
    }

    fn load_metadata(&mut self) -> LoadMetadata<'a> {
        if self.inner.config.metadata_max_age > 0 {
            let handle = self.inner.handle.clone();

            let timeout = Timeout::new(self.inner.config.metadata_max_age(), &handle);

            match timeout {
                Ok(timeout) => {
                    let inner = self.inner.clone();
                    let future = timeout
                        .map_err(Error::from)
                        .and_then(move |_| LoadMetadata::new(inner.clone()))
                        .map(|_| ())
                        .map_err(|_| ());

                    handle.spawn(future);
                }
                Err(err) => {
                    warn!("fail to create timeout, {}", err);
                }
            }
        }

        LoadMetadata::new(self.inner.clone())
    }

    fn group_coordinator(&self, group_id: Cow<'a, str>) -> GroupCoordinator {
        let inner = self.inner.clone();
        let future = self.metadata()
            .and_then(move |metadata| inner.group_coordinator(metadata, group_id));
        GroupCoordinator::new(future)
    }

    fn join_group(&self,
                  coordinator: BrokerRef,
                  group_id: Cow<'a, str>,
                  session_timeout: i32,
                  rebalance_timeout: i32,
                  member_id: Cow<'a, str>,
                  protocol_type: Cow<'a, str>,
                  group_protocols: Vec<ConsumerGroupProtocol<'a>>)
                  -> JoinGroup {
        let inner = self.inner.clone();
        let future = self.metadata()
            .and_then(move |metadata| {
                metadata
                    .find_broker(coordinator)
                    .map(move |coordinator| {
                        inner.join_group(coordinator,
                                         group_id,
                                         session_timeout,
                                         rebalance_timeout,
                                         member_id,
                                         protocol_type,
                                         group_protocols)
                    })
                    .unwrap_or_else(|| ErrorKind::BrokerNotFound(coordinator).into())
            });
        JoinGroup::new(future)
    }

    fn heartbeat(&self, coordinator: BrokerRef, generation: Generation) -> Heartbeat {
        let inner = self.inner.clone();
        let future = self.metadata()
            .and_then(move |metadata| {
                metadata
                    .find_broker(coordinator)
                    .map(move |coordinator| {
                             inner.heartbeat(coordinator,
                                             generation.group_id.into(),
                                             generation.generation_id,
                                             generation.member_id.into())
                         })
                    .unwrap_or_else(|| ErrorKind::BrokerNotFound(coordinator).into())
            });
        Heartbeat::new(future)
    }

    fn leave_group(&self, coordinator: BrokerRef, generation: Generation) -> LeaveGroup {
        let inner = self.inner.clone();
        let future = self.metadata()
            .and_then(move |metadata| {
                metadata
                    .find_broker(coordinator)
                    .map(move |coordinator| {
                             inner.leave_group(coordinator,
                                               generation.group_id.into(),
                                               generation.member_id.into())
                         })
                    .unwrap_or_else(|| ErrorKind::BrokerNotFound(coordinator).into())
            });
        LeaveGroup::new(future)
    }

    fn sync_group(&self,
                  coordinator: BrokerRef,
                  generation: Generation,
                  group_assignment: Option<Vec<ConsumerGroupAssignment<'a>>>)
                  -> SyncGroup {
        let inner = self.inner.clone();
        let future = self.metadata()
            .and_then(move |metadata| {
                metadata
                    .find_broker(coordinator)
                    .map(move |coordinator| {
                             inner.sync_group(coordinator,
                                              generation.group_id.into(),
                                              generation.generation_id,
                                              generation.member_id.into(),
                                              group_assignment)
                         })
                    .unwrap_or_else(|| ErrorKind::BrokerNotFound(coordinator).into())
            });
        SyncGroup::new(future)
    }
}

impl<'a> Inner<'a>
    where Self: 'static
{
    fn next_correlation_id(&self) -> CorrelationId {
        (*self.state).borrow_mut().next_correlation_id()
    }

    fn client_id(&self) -> Option<Cow<'a, str>> {
        self.config.client_id.clone().map(Cow::from)
    }

    pub fn metadata(&self) -> GetMetadata {
        (*self.state).borrow().metadata()
    }

    /// Choose the node with the fewest outstanding requests which is at least eligible for connection.
    pub fn least_loaded_broker(&self, metadata: Rc<Metadata>) -> Result<(SocketAddr, BrokerRef)> {
        let mut brokers = metadata.brokers().to_vec();

        rand::thread_rng().shuffle(&mut brokers);

        let mut in_flight_requests = usize::max_value();
        let mut found = None;

        for broker in brokers {
            for addr in broker.addr().to_socket_addrs()? {
                match self.service.in_flight_requests(&addr) {
                    Some(0) => {
                        trace!("found least loaded broker #{} @ {} without in flight requests",
                               broker.id(),
                               addr);

                        return Ok((addr, broker.as_ref()));
                    }
                    Some(n) if n < in_flight_requests => {
                        in_flight_requests = n;
                        found = Some((addr, broker.as_ref()));
                    }
                    _ => {}
                }
            }
        }

        found
            .map(|(addr, broker)| {
                     trace!("found least loaded broker #{} @ {} with {} in flight requests",
                            broker.index(),
                            addr,
                            self.service.in_flight_requests(&addr).unwrap_or_default());

                     (addr, broker)
                 })
            .or_else(|| {
                metadata
                    .brokers()
                    .first()
                    .map(|broker| {
                        let addr = broker.addr().to_socket_addrs().unwrap().next().unwrap();

                        trace!("not found any alive broker, use a random broker # {} @ {}",
                               broker.id(),
                               addr);

                        (addr, broker.as_ref())
                    })
            })
            .ok_or_else(|| {
                            warn!("not found any broker");

                            ErrorKind::KafkaError(KafkaCode::BrokerNotAvailable).into()
                        })
    }

    fn fetch_metadata<S>(&self, topic_names: &[S]) -> FetchMetadata
        where S: AsRef<str> + Debug
    {
        debug!("fetch metadata for toipcs: {:?}", topic_names);

        let responses = {
            let mut responses = Vec::new();

            for addr in &self.config.hosts {
                let request = KafkaRequest::fetch_metadata(0, // api_version
                                                           self.next_correlation_id(),
                                                           self.client_id(),
                                                           topic_names);

                let response = self.service
                    .call((*addr, request))
                    .and_then(|res| if let KafkaResponse::Metadata(res) = res {
                                  future::ok(Rc::new(Metadata::from(res)))
                              } else {
                                  future::err(ErrorKind::UnexpectedResponse(res.api_key()).into())
                              });

                responses.push(response);
            }

            responses
        };

        FetchMetadata::new(future::select_ok(responses).map(|(metadata, _)| metadata))
    }

    fn fetch_api_versions(&self, broker: &Broker) -> FetchApiVersions {
        debug!("fetch API versions for broker: {:?}", broker);

        let addr = broker.addr().to_socket_addrs().unwrap().next().unwrap(); // TODO

        let request = KafkaRequest::api_versions(self.next_correlation_id(), self.client_id());

        let response = self.service
            .call((addr, request))
            .and_then(|res| if let KafkaResponse::ApiVersions(res) = res {
                          future::ok(UsableApiVersions::new(res.api_versions))
                      } else {
                          future::err(ErrorKind::UnexpectedResponse(res.api_key()).into())
                      });

        FetchApiVersions::new(response)
    }

    fn load_api_versions(&self, metadata: Rc<Metadata>) -> LoadApiVersions {
        trace!("load API versions from brokers, {:?}", metadata.brokers());

        let responses = {
            let mut responses = Vec::new();

            for broker in metadata.brokers() {
                let broker_ref = broker.as_ref();
                let response = self.fetch_api_versions(broker)
                    .map(move |api_versions| (broker_ref, api_versions));

                responses.push(response);
            }

            responses
        };
        let responses = future::join_all(responses).map(HashMap::from_iter);

        LoadApiVersions::new(StaticBoxFuture::new(responses))
    }

    fn produce_records(&self,
                       metadata: Rc<Metadata>,
                       required_acks: RequiredAcks,
                       timeout: Duration,
                       tp: TopicPartition<'a>,
                       records: Vec<Cow<'a, MessageSet>>)
                       -> ProduceRecords {
        let (api_version, addr) = metadata
            .leader_for(&tp)
            .map_or_else(|| (0, *self.config.hosts.iter().next().unwrap()),
                         |broker| {
                             (broker.api_version(ApiKeys::Produce).unwrap_or_default(),
                              broker.addr().to_socket_addrs().unwrap().next().unwrap())
                         });

        let request = KafkaRequest::produce_records(api_version,
                                                    self.next_correlation_id(),
                                                    self.client_id(),
                                                    required_acks,
                                                    timeout,
                                                    &tp,
                                                    records);

        let response = self.service
            .call((addr, request))
            .and_then(|res| if let KafkaResponse::Produce(res) = res {
                          let produce = res.topics
                              .iter()
                              .map(|topic| {
                    (topic.topic_name.to_owned(),
                     topic
                         .partitions
                         .iter()
                         .map(|partition| {
                                  (partition.partition, partition.error_code, partition.offset)
                              })
                         .collect())
                })
                              .collect();

                          future::ok(produce)
                      } else {
                          future::err(ErrorKind::UnexpectedResponse(res.api_key()).into())
                      });

        ProduceRecords::new(response)
    }

    fn topics_by_broker(&self,
                        metadata: Rc<Metadata>,
                        topic_partitions: Vec<TopicPartition<'a>>)
                        -> Topics<'a> {
        let mut topics = HashMap::new();

        for topic_partition in topic_partitions {
            if let Some(broker) = metadata.leader_for(&topic_partition) {
                let addr = broker.addr().to_socket_addrs().unwrap().next().unwrap(); // TODO
                let api_version = broker.api_version(ApiKeys::ListOffsets).unwrap_or_default();
                topics
                    .entry((addr, api_version))
                    .or_insert_with(HashMap::new)
                    .entry(topic_partition.topic_name)
                    .or_insert_with(Vec::new)
                    .push(topic_partition.partition);
            }
        }

        topics
    }

    fn fetch_offsets(&self, topics: Topics<'a>, offset: FetchOffset) -> FetchOffsets {
        let responses = {
            let mut responses = Vec::new();

            for ((addr, api_version), topics) in topics {
                let request = KafkaRequest::list_offsets(api_version,
                                                         self.next_correlation_id(),
                                                         self.client_id(),
                                                         topics,
                                                         offset);
                let response = self.service
                    .call((addr, request))
                    .and_then(|res| {
                        if let KafkaResponse::ListOffsets(res) = res {
                            let topics = res.topics
                                .iter()
                                .map(|topic| {
                                    let partitions = topic
                                        .partitions
                                        .iter()
                                        .flat_map(|partition| {
                                            if partition.error_code ==
                                               KafkaCode::None as ErrorCode {
                                                Ok(PartitionOffset {
                                                       partition: partition.partition,
                                                       offset: *partition
                                                                    .offsets
                                                                    .iter()
                                                                    .next()
                                                                    .unwrap(), // TODO
                                                   })
                                            } else {
                                                Err(ErrorKind::KafkaError(partition
                                                                              .error_code
                                                                              .into()))
                                            }
                                        })
                                        .collect();

                                    (topic.topic_name.clone(), partitions)
                                })
                                .collect::<Vec<(String, Vec<PartitionOffset>)>>();

                            Ok(topics)
                        } else {
                            bail!(ErrorKind::UnexpectedResponse(res.api_key()))
                        }
                    });

                responses.push(response);
            }

            responses
        };

        let offsets = future::join_all(responses).map(|responses| {
            responses
                .iter()
                .fold(HashMap::new(), |mut offsets, topics| {
                    for &(ref topic_name, ref partitions) in topics {
                        offsets
                            .entry(topic_name.clone())
                            .or_insert_with(Vec::new)
                            .extend(partitions.iter().cloned())
                    }
                    offsets
                })
        });

        FetchOffsets::new(offsets)
    }

    fn group_coordinator(&self,
                         metadata: Rc<Metadata>,
                         group_id: Cow<'a, str>)
                         -> GroupCoordinator {
        debug!("disover group coordinator of group `{}`", group_id);

        let addr = {
            match self.least_loaded_broker(metadata) {
                Ok((addr, _)) => addr,
                Err(err) => {
                    return GroupCoordinator::err(err);
                }
            }
        };

        let request = KafkaRequest::group_coordinator(0, // api_version,
                                                      self.next_correlation_id(),
                                                      self.client_id(),
                                                      group_id);

        let response = self.service
            .call((addr, request))
            .and_then(|res| if let KafkaResponse::GroupCoordinator(res) = res {
                          if res.error_code == KafkaCode::None as ErrorCode {
                              future::ok(Broker::new(res.coordinator_id,
                                                     &res.coordinator_host,
                                                     res.coordinator_port as u16))
                          } else {
                              future::err(ErrorKind::KafkaError(res.error_code.into()).into())
                          }
                      } else {
                          future::err(ErrorKind::UnexpectedResponse(res.api_key()).into())
                      });

        GroupCoordinator::new(response)
    }

    fn join_group(&self,
                  coordinator: &Broker,
                  group_id: Cow<'a, str>,
                  session_timeout: i32,
                  rebalance_timeout: i32,
                  member_id: Cow<'a, str>,
                  protocol_type: Cow<'a, str>,
                  group_protocols: Vec<ConsumerGroupProtocol<'a>>)
                  -> JoinGroup {
        debug!("member `{}` join group `{}`", member_id, group_id);

        let addr = coordinator
            .addr()
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap(); // TODO

        let api_version = coordinator
            .api_version(ApiKeys::JoinGroup)
            .unwrap_or_default();

        let joined_group_id: String = (*group_id).to_owned();

        let request = KafkaRequest::join_group(api_version,
                                               self.next_correlation_id(),
                                               self.client_id(),
                                               group_id,
                                               session_timeout,
                                               rebalance_timeout,
                                               member_id,
                                               protocol_type,
                                               group_protocols);

        let response = self.service
            .call((addr, request))
            .and_then(move |res| if let KafkaResponse::JoinGroup(res) = res {
                          if res.error_code == KafkaCode::None as ErrorCode {
                              future::ok(ConsumerGroup {
                                             group_id: joined_group_id,
                                             generation_id: res.generation_id,
                                             protocol: res.protocol,
                                             leader_id: res.leader_id,
                                             member_id: res.member_id,
                                             members: res.members,
                                         })
                          } else {
                              future::err(ErrorKind::KafkaError(res.error_code.into()).into())
                          }
                      } else {
                          future::err(ErrorKind::UnexpectedResponse(res.api_key()).into())
                      });

        JoinGroup::new(response)
    }

    fn heartbeat(&self,
                 coordinator: &Broker,
                 group_id: Cow<'a, str>,
                 group_generation_id: GenerationId,
                 member_id: Cow<'a, str>)
                 -> Heartbeat {
        debug!("member `{}` send heartbeat to group `{}`",
               member_id,
               group_id);

        let addr = coordinator
            .addr()
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap(); // TODO

        let request = KafkaRequest::heartbeat(self.next_correlation_id(),
                                              self.client_id(),
                                              group_id,
                                              group_generation_id,
                                              member_id);

        let response = self.service
            .call((addr, request))
            .and_then(move |res| if let KafkaResponse::Heartbeat(res) = res {
                          if res.error_code == KafkaCode::None as ErrorCode {
                              future::ok(())
                          } else {
                              future::err(ErrorKind::KafkaError(res.error_code.into()).into())
                          }
                      } else {
                          future::err(ErrorKind::UnexpectedResponse(res.api_key()).into())
                      });

        Heartbeat::new(response)
    }

    fn leave_group(&self,
                   coordinator: &Broker,
                   group_id: Cow<'a, str>,
                   member_id: Cow<'a, str>)
                   -> LeaveGroup {
        debug!("member `{}` leave group `{}`", member_id, group_id);

        let addr = coordinator
            .addr()
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap(); // TODO

        let leaved_group_id: String = (*group_id).to_owned();

        let request = KafkaRequest::leave_group(self.next_correlation_id(),
                                                self.client_id(),
                                                group_id,
                                                member_id);

        let response = self.service
            .call((addr, request))
            .and_then(move |res| if let KafkaResponse::LeaveGroup(res) = res {
                          if res.error_code == KafkaCode::None as ErrorCode {
                              future::ok(leaved_group_id)
                          } else {
                              future::err(ErrorKind::KafkaError(res.error_code.into()).into())
                          }
                      } else {
                          future::err(ErrorKind::UnexpectedResponse(res.api_key()).into())
                      });

        LeaveGroup::new(response)
    }

    fn sync_group(&self,
                  coordinator: &Broker,
                  group_id: Cow<'a, str>,
                  group_generation_id: GenerationId,
                  member_id: Cow<'a, str>,
                  group_assignment: Option<Vec<ConsumerGroupAssignment<'a>>>)
                  -> SyncGroup {
        debug!("sync group `{}` # {} with member `{}`",
               group_id,
               group_generation_id,
               member_id);

        let addr = coordinator
            .addr()
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap(); // TODO

        let request = KafkaRequest::sync_group(self.next_correlation_id(),
                                               self.client_id(),
                                               group_id,
                                               group_generation_id,
                                               member_id,
                                               group_assignment.unwrap_or_default());

        let response = self.service
            .call((addr, request))
            .and_then(move |res| if let KafkaResponse::SyncGroup(res) = res {
                          if res.error_code == KafkaCode::None as ErrorCode {
                              future::ok(res.member_assignment)
                          } else {
                              future::err(ErrorKind::KafkaError(res.error_code.into()).into())
                          }
                      } else {
                          future::err(ErrorKind::UnexpectedResponse(res.api_key()).into())
                      });

        SyncGroup::new(response)
    }
}

type Topics<'a> = HashMap<(SocketAddr, ApiVersion), HashMap<Cow<'a, str>, Vec<PartitionId>>>;

impl State {
    pub fn next_correlation_id(&mut self) -> CorrelationId {
        self.correlation_id = self.correlation_id.wrapping_add(1);
        self.correlation_id - 1
    }

    pub fn metadata(&self) -> GetMetadata {
        let (sender, receiver) = oneshot::channel();

        match self.metadata_status {
            MetadataStatus::Loading(ref senders) => senders.borrow_mut().push(sender),
            MetadataStatus::Loaded(ref metadata) => drop(sender.send(metadata.clone())),
        }

        GetMetadata::new(receiver.map_err(|_| ErrorKind::Canceled("load metadata canceled").into()))
    }

    pub fn refresh_metadata(&mut self) {
        if let MetadataStatus::Loaded(_) = self.metadata_status {
            self.metadata_status = MetadataStatus::Loading(Default::default());
        }
    }

    pub fn update_metadata(&mut self, metadata: Rc<Metadata>) {
        let status = mem::replace(&mut self.metadata_status,
                                  MetadataStatus::Loaded(metadata.clone()));

        if let MetadataStatus::Loading(senders) = status {
            for sender in senders.into_inner() {
                drop(sender.send(metadata.clone()));
            }
        }
    }
}

/// The future of loaded metadata
pub struct LoadMetadata<'a> {
    state: Loading,
    inner: Rc<Inner<'a>>,
}

pub enum Loading {
    Metadata(FetchMetadata),
    ApiVersions(Rc<Metadata>, LoadApiVersions),
    Finished(Rc<Metadata>),
}

impl<'a> LoadMetadata<'a>
    where Self: 'static
{
    fn new(inner: Rc<Inner<'a>>) -> LoadMetadata<'a> {
        let fetch_metadata = inner.fetch_metadata::<&str>(&[]);

        (*inner.state).borrow_mut().refresh_metadata();

        LoadMetadata {
            state: Loading::Metadata(fetch_metadata),
            inner: inner,
        }
    }
}

impl<'a> Future for LoadMetadata<'a>
    where Self: 'static
{
    type Item = Rc<Metadata>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let state;

            match self.state {
                Loading::Metadata(ref mut future) => {
                    match future.poll() {
                        Ok(Async::Ready(metadata)) => {
                            let inner = self.inner.clone();

                            if inner.config.api_version_request {
                                state = Loading::ApiVersions(metadata.clone(),
                                                             inner.load_api_versions(metadata));
                            } else {
                                let fallback_api_versions =
                                    inner.config.broker_version_fallback.api_versions();

                                let metadata = Rc::new(metadata.with_fallback_api_versions(fallback_api_versions));

                                trace!("use fallback API versions from {:?}, {:?}",
                                       inner.config.broker_version_fallback,
                                       fallback_api_versions);

                                state = Loading::Finished(metadata);
                            }
                        }
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Err(err) => return Err(err),
                    }
                }
                Loading::ApiVersions(ref metadata, ref mut future) => {
                    match future.poll() {
                        Ok(Async::Ready(api_versions)) => {
                            let metadata = Rc::new(metadata.with_api_versions(api_versions));

                            state = Loading::Finished(metadata);
                        }
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Err(err) => return Err(err),
                    }
                }
                Loading::Finished(ref metadata) => {
                    (*self.inner.state)
                        .borrow_mut()
                        .update_metadata(metadata.clone());

                    return Ok(Async::Ready(metadata.clone()));
                }
            }

            self.state = state;
        }
    }
}


pub struct StaticBoxFuture<T = (), E = Error>(Box<Future<Item = T, Error = E> + 'static>)
    where T: 'static,
          E: 'static;

impl<T, E> StaticBoxFuture<T, E> {
    pub fn new<F>(inner: F) -> Self
        where F: IntoFuture<Item = T, Error = E> + 'static,
              T: 'static,
              E: 'static
    {
        StaticBoxFuture(Box::new(inner.into_future()))
    }

    pub fn ok(item: T) -> Self {
        StaticBoxFuture(Box::new(future::ok(item)))
    }

    pub fn err(err: E) -> Self {
        StaticBoxFuture(Box::new(future::err(err)))
    }
}

impl<T, E> From<ErrorKind> for StaticBoxFuture<T, E>
    where E: From<ErrorKind>
{
    fn from(err: ErrorKind) -> Self {
        Self::err(err.into())
    }
}

impl<T, E> Future for StaticBoxFuture<T, E> {
    type Item = T;
    type Error = E;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll()
    }
}

pub type GetMetadata = StaticBoxFuture<Rc<Metadata>>;
pub type FetchMetadata = StaticBoxFuture<Rc<Metadata>>;
pub type FetchApiVersions = StaticBoxFuture<UsableApiVersions>;
pub type LoadApiVersions = StaticBoxFuture<HashMap<BrokerRef, UsableApiVersions>>;

#[derive(Clone)]
struct InFlightMiddleware<S> {
    upstream: S,
    requests: HashMap<SocketAddr, usize>,
}

impl<S> InFlightMiddleware<S> {
    pub fn new(upstream: S) -> InFlightMiddleware<S> {
        InFlightMiddleware {
            upstream: upstream,
            requests: HashMap::new(),
        }
    }

    pub fn in_flight_requests(&self, addr: &SocketAddr) -> Option<usize> {
        self.requests.get(addr).cloned()
    }

    pub fn send_request(&mut self, addr: SocketAddr) {
        let requests = self.requests.entry(addr).or_insert(0);

        if let Some(new) = requests.checked_add(1) {
            *requests = new;
        }
    }

    pub fn received_response(&mut self, addr: SocketAddr) {
        let requests = self.requests.entry(addr).or_insert(0);

        if let Some(new) = requests.checked_sub(1) {
            *requests = new;
        }
    }
}

impl<S> Service for InFlightMiddleware<S>
    where S: Service
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn call(&self, request: Self::Request) -> Self::Future {
        self.upstream.call(request)
    }
}
