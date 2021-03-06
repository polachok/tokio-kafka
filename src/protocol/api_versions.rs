use std::ops::Deref;
use std::str::FromStr;
use std::mem;

use bytes::{ByteOrder, BytesMut};

use nom::{IResult, be_i16, be_i32};

use errors::{Error, Result};
use protocol::{parse_response_header, ApiKeys, ApiVersion, Encodable, ErrorCode, ParseTag, Record, RecordFormat,
               RequestHeader, ResponseHeader};

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(i16)]
pub enum ApiVersions {
    KAFKA_0_8_0,
    KAFKA_0_8_1,
    KAFKA_0_8_2,
    KAFKA_0_9_0,
    /// 0.10.0-IV0 is introduced for KIP-31/32 which changes the message format.
    KAFKA_0_10_0_IV0,
    /// 0.10.0-IV1 is introduced for KIP-36(rack awareness) and KIP-43(SASL
    /// handshake).
    KAFKA_0_10_0_IV1,
    /// introduced for JoinGroup protocol change in KIP-62
    KAFKA_0_10_1_IV0,
    /// 0.10.1-IV1 is introduced for KIP-74(fetch response size limit).
    KAFKA_0_10_1_IV1,
    /// introduced ListOffsetRequest v1 in KIP-79
    KAFKA_0_10_1_IV2,
    /// introduced UpdateMetadataRequest v3 in KIP-103
    KAFKA_0_10_2_IV0,
    /// KIP-98 (idempotent and transactional producer support)
    KAFKA_0_11_0_IV0,
    /// introduced DeleteRecordsRequest v0 and FetchRequest v4 in KIP-107
    KAFKA_0_11_0_IV1,
    /// Introduced leader epoch fetches to the replica fetcher via KIP-101
    KAFKA_0_11_0_IV2,
    /// Introduced LeaderAndIsrRequest V1, UpdateMetadataRequest V4 and
    /// FetchRequest V6 via KIP-112
    KAFKA_1_0_IV0,
    /// Introduced DeleteGroupsRequest V0 via KIP-229, plus KIP-227 incremental fetch requests,
    /// and KafkaStorageException for fetch requests.
    KAFKA_1_1_IV0,
}

impl ApiVersions {
    pub fn min_version(record_fmt: RecordFormat) -> Self {
        match record_fmt {
            RecordFormat::V0 => ApiVersions::KAFKA_0_8_0,
            RecordFormat::V1 => ApiVersions::KAFKA_0_10_0_IV1,
            RecordFormat::V2 => ApiVersions::KAFKA_0_11_0_IV2,
        }
    }
}

impl From<ApiVersion> for ApiVersions {
    fn from(version: ApiVersion) -> Self {
        unsafe { mem::transmute(version) }
    }
}

impl FromStr for ApiVersions {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        parse_api_versions(s.as_bytes()).to_result().map_err(|err| err.into())
    }
}

named!(
    parse_api_versions<ApiVersions>,
    alt_complete!(
        tag!("0.8.0")       => { |_| ApiVersions::KAFKA_0_8_0 } |
        tag!("0.8.1")       => { |_| ApiVersions::KAFKA_0_8_1 } |
        tag!("0.8.2")       => { |_| ApiVersions::KAFKA_0_8_2 } |
        tag!("0.9.0")       => { |_| ApiVersions::KAFKA_0_9_0 } |
        tag!("0.10.0-IV0")  => { |_| ApiVersions::KAFKA_0_10_0_IV0 } |
        tag!("0.10.0-IV1")  => { |_| ApiVersions::KAFKA_0_10_0_IV1 } |
        tag!("0.10.0")      => { |_| ApiVersions::KAFKA_0_10_0_IV1 } |
        tag!("0.10.1-IV0")  => { |_| ApiVersions::KAFKA_0_10_1_IV0 } |
        tag!("0.10.1-IV1")  => { |_| ApiVersions::KAFKA_0_10_1_IV1 } |
        tag!("0.10.1-IV2")  => { |_| ApiVersions::KAFKA_0_10_1_IV2 } |
        tag!("0.10.1")      => { |_| ApiVersions::KAFKA_0_10_1_IV2 } |
        tag!("0.10.2-IV0")  => { |_| ApiVersions::KAFKA_0_10_2_IV0 } |
        tag!("0.10.2")      => { |_| ApiVersions::KAFKA_0_10_2_IV0 } |
        tag!("0.11.0-IV0")  => { |_| ApiVersions::KAFKA_0_11_0_IV0 } |
        tag!("0.11.0-IV1")  => { |_| ApiVersions::KAFKA_0_11_0_IV1 } |
        tag!("0.11.0-IV2")  => { |_| ApiVersions::KAFKA_0_11_0_IV2 } |
        tag!("0.11.0")      => { |_| ApiVersions::KAFKA_0_11_0_IV2 } |
        tag!("1.0-IV0")     => { |_| ApiVersions::KAFKA_1_0_IV0 } |
        tag!("1.0")         => { |_| ApiVersions::KAFKA_1_0_IV0 } |
        tag!("1.1-IV0")     => { |_| ApiVersions::KAFKA_1_1_IV0 } |
        tag!("1.1")         => { |_| ApiVersions::KAFKA_1_1_IV0 }
    )
);

// api versions we support
lazy_static! {
    pub static ref SUPPORTED_API_VERSIONS: UsableApiVersions = UsableApiVersions::new(vec![
        UsableApiVersion {
            api_key: ApiKeys::Produce,
            min_version: 0,
            max_version: 1,
        },
        UsableApiVersion {
            api_key: ApiKeys::Fetch,
            min_version: 0,
            max_version: 3,
        },
        UsableApiVersion {
            api_key: ApiKeys::ListOffsets,
            min_version: 0,
            max_version: 1,
        },
        UsableApiVersion {
            api_key: ApiKeys::OffsetCommit,
            min_version: 0,
            max_version: 2,
        },
        UsableApiVersion {
            api_key: ApiKeys::OffsetFetch,
            min_version: 0,
            max_version: 1,
        },
        UsableApiVersion {
            api_key: ApiKeys::JoinGroup,
            min_version: 0,
            max_version: 1,
        }
    ]);
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApiVersionsRequest<'a> {
    pub header: RequestHeader<'a>,
}

impl<'a> Record for ApiVersionsRequest<'a> {
    fn size(&self, api_version: ApiVersion) -> usize {
        self.header.size(api_version)
    }
}

impl<'a> Encodable for ApiVersionsRequest<'a> {
    fn encode<T: ByteOrder>(&self, dst: &mut BytesMut) -> Result<()> {
        self.header.encode::<T>(dst)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApiVersionsResponse {
    pub header: ResponseHeader,
    /// Error code.
    pub error_code: ErrorCode,
    /// API versions supported by the broker.
    pub api_versions: Vec<UsableApiVersion>,
}

/// API versions supported by the broker.
#[derive(Clone, Debug, PartialEq)]
pub struct UsableApiVersion {
    /// API key.
    pub api_key: ApiKeys,
    /// Minimum supported version.
    pub min_version: ApiVersion,
    /// Maximum supported version.
    pub max_version: ApiVersion,
}

/// Maintains node api versions for access outside of Client
#[derive(Clone, Debug)]
pub struct UsableApiVersions(Vec<UsableApiVersion>);

impl Deref for UsableApiVersions {
    type Target = [UsableApiVersion];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl UsableApiVersions {
    /// Construct a new UsableApiVersions
    pub fn new(api_versions: Vec<UsableApiVersion>) -> Self {
        UsableApiVersions(api_versions)
    }

    /// Find API version for the given ApiKeys
    pub fn find(&self, api_key: ApiKeys) -> Option<&UsableApiVersion> {
        self.0.iter().find(|v| v.api_key == api_key)
    }
}

impl ApiVersionsResponse {
    pub fn parse(buf: &[u8]) -> IResult<&[u8], Self> {
        parse_api_versions_response(buf)
    }
}

named!(
    parse_api_versions_response<ApiVersionsResponse>,
    parse_tag!(
        ParseTag::ApiVersionsResponse,
        do_parse!(
            header: parse_response_header >> error_code: be_i16
                >> api_versions: length_count!(be_i32, parse_api_version) >> (ApiVersionsResponse {
                header,
                error_code,
                api_versions,
            })
        )
    )
);

named!(
    parse_api_version<UsableApiVersion>,
    parse_tag!(
        ParseTag::ApiVersion,
        do_parse!(
            api_key: be_i16 >> min_version: be_i16 >> max_version: be_i16 >> (UsableApiVersion {
                api_key: ApiKeys::from(api_key),
                min_version,
                max_version,
            })
        )
    )
);

#[cfg(test)]
mod tests {
    use bytes::{BigEndian, BytesMut};

    use nom::IResult;

    use super::*;
    use protocol::*;

    lazy_static!{
        static ref TEST_REQUEST_DATA: Vec<u8> = vec![
            // ApiVersionsRequest
                // RequestHeader
                0, 18,                              // api_key
                0, 0,                               // api_version
                0, 0, 0, 123,                       // correlation_id
                0, 6, 99, 108, 105, 101, 110, 116,  // client_id
        ];

        static ref TEST_RESPONSE_DATA: Vec<u8> = vec![
            // ResponseHeader
            0, 0, 0, 123,   // correlation_id
            0, 0,           // error_code
            // api_versions: [ApiVersion]
            0, 0, 0, 1,
                0, 1,       // api_key
                0, 2,       // min_version
                0, 3,       // max_version
        ];

        static ref TEST_RESPONSE: ApiVersionsResponse = ApiVersionsResponse {
            header: ResponseHeader { correlation_id: 123 },
            error_code: 0,
            api_versions: vec![UsableApiVersion {
                api_key: ApiKeys::Fetch,
                min_version: 2,
                max_version: 3,
            }],
        };
    }

    #[test]
    fn test_parse_api_versions() {
        let api_versions = vec![
            ("0.8.0", ApiVersions::KAFKA_0_8_0),
            ("0.8.0.0", ApiVersions::KAFKA_0_8_0),
            ("0.8.2", ApiVersions::KAFKA_0_8_2),
            ("0.9.0", ApiVersions::KAFKA_0_9_0),
            ("0.10.0-IV0", ApiVersions::KAFKA_0_10_0_IV0),
            ("0.10.0-IV1", ApiVersions::KAFKA_0_10_0_IV1),
            ("0.10.0", ApiVersions::KAFKA_0_10_0_IV1),
            ("0.10.1-IV0", ApiVersions::KAFKA_0_10_1_IV0),
            ("0.10.1-IV1", ApiVersions::KAFKA_0_10_1_IV1),
            ("0.10.1-IV2", ApiVersions::KAFKA_0_10_1_IV2),
            ("0.10.1", ApiVersions::KAFKA_0_10_1_IV2),
            ("0.10.2-IV0", ApiVersions::KAFKA_0_10_2_IV0),
            ("0.10.2", ApiVersions::KAFKA_0_10_2_IV0),
            ("0.11.0-IV0", ApiVersions::KAFKA_0_11_0_IV0),
            ("0.11.0-IV1", ApiVersions::KAFKA_0_11_0_IV1),
            ("0.11.0-IV2", ApiVersions::KAFKA_0_11_0_IV2),
            ("0.11.0", ApiVersions::KAFKA_0_11_0_IV2),
            ("1.0-IV0", ApiVersions::KAFKA_1_0_IV0),
            ("1.0", ApiVersions::KAFKA_1_0_IV0),
            ("1.1-IV0", ApiVersions::KAFKA_1_1_IV0),
            ("1.1", ApiVersions::KAFKA_1_1_IV0),
        ];

        for (s, version) in api_versions {
            let res: Result<ApiVersions> = s.parse();

            assert_eq!(res.unwrap(), version, "parse API version: {}", s);
        }
    }

    #[test]
    fn test_encode_api_versions_request() {
        let req = ApiVersionsRequest {
            header: RequestHeader {
                api_key: ApiKeys::ApiVersions as ApiKey,
                api_version: 0,
                correlation_id: 123,
                client_id: Some("client".into()),
            },
        };

        let mut buf = BytesMut::with_capacity(128);

        req.encode::<BigEndian>(&mut buf).unwrap();

        assert_eq!(req.size(req.header.api_version), buf.len());

        assert_eq!(&buf[..], &TEST_REQUEST_DATA[..]);
    }

    #[test]
    fn test_parse_api_versions_response() {
        assert_eq!(
            parse_api_versions_response(TEST_RESPONSE_DATA.as_slice()),
            IResult::Done(&[][..], TEST_RESPONSE.clone())
        );
    }
}
