use bytes::{BytesMut, BufMut, ByteOrder};

use nom::{be_i16, be_i32, be_i64};

use errors::Result;
use protocol::{Encodable, RequestHeader, ResponseHeader, MessageSet, parse_message_set, ParseTag,
               parse_string, parse_response_header, WriteExt};

#[derive(Clone, Debug, PartialEq)]
pub struct FetchRequest {
    pub header: RequestHeader,
    /// The replica id indicates the node id of the replica initiating this request.
    pub replica_id: i32,
    /// The maximum amount of time in milliseconds to block waiting if insufficient data is available at the time the request is issued.
    pub max_wait_time: i32,
    /// This is the minimum number of bytes of messages that must be available to give a response.
    pub min_bytes: i32,
    pub topics: Vec<FetchTopic>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FetchTopic {
    /// The name of the topic.
    pub topic_name: String,
    pub partitions: Vec<FetchPartition>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FetchPartition {
    /// The id of the partition the fetch is for.
    pub partition: i32,
    /// The offset to begin this fetch from.
    pub fetch_offset: i64,
    /// The maximum bytes to include in the message set for this partition.
    pub max_bytes: i32,
}

impl Encodable for FetchRequest {
    fn encode<T: ByteOrder>(self, dst: &mut BytesMut) -> Result<()> {
        self.header.encode::<T>(dst)?;

        dst.put_i32::<T>(self.replica_id);
        dst.put_i32::<T>(self.max_wait_time);
        dst.put_i32::<T>(self.min_bytes);
        dst.put_array::<T, _, _>(self.topics, |buf, topic| {
            buf.put_str::<T, _>(Some(topic.topic_name))?;
            buf.put_array::<T, _, _>(topic.partitions, |buf, partition| {
                buf.put_i32::<T>(partition.partition);
                buf.put_i64::<T>(partition.fetch_offset);
                buf.put_i32::<T>(partition.max_bytes);
                Ok(())
            })
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FetchResponse {
    pub header: ResponseHeader,
    /// Duration in milliseconds for which the request was throttled due to quota violation.
    pub throttle_time: Option<i32>,
    pub topics: Vec<TopicData>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TopicData {
    /// The name of the topic this response entry is for.
    pub topic_name: String,
    pub partitions: Vec<PartitionData>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PartitionData {
    /// The id of the partition the fetch is for.
    pub partition: i32,
    pub error_code: i16,
    ///The offset at the end of the log for this partition.
    pub highwater_mark_offset: i64,
    pub message_set: MessageSet,
}

named_args!(pub parse_fetch_response(api_version: i16)<FetchResponse>,
    do_parse!(
        header: parse_response_header
     >> throttle_time: cond!(api_version > 0, be_i32)
     >> topics: parse_tag!(ParseTag::FetchTopics,
            length_count!(be_i32, apply!(parse_fetch_topic_data, api_version)))
     >> (FetchResponse {
            header: header,
            throttle_time: throttle_time,
            topics: topics,
        })
    )
);

named_args!(parse_fetch_topic_data(api_version: i16)<TopicData>,
    do_parse!(
        topic_name: parse_string
     >> partitions: parse_tag!(ParseTag::FetchPartitions,
            length_count!(be_i32, apply!(parse_fetch_partition_data, api_version)))
     >> (TopicData {
            topic_name: topic_name,
            partitions: partitions,
        })
    )
);

named_args!(parse_fetch_partition_data(api_version: i16)<PartitionData>,
    do_parse!(
        partition: be_i32
     >> error_code: be_i16
     >> offset: be_i64
     >> message_set: length_value!(be_i32, apply!(parse_message_set, api_version))
     >> (PartitionData {
            partition: partition,
            error_code: error_code,
            highwater_mark_offset: offset,
            message_set: message_set,
        })
    )
);
