use bytes::{ByteOrder, BytesMut};

use nom::{be_i16, be_i32};

use errors::Result;
use protocol::{ApiKey, ApiKeys, ApiVersion, Encodable, ErrorCode, ParseTag, RequestHeader,
               ResponseHeader, parse_response_header};

#[derive(Clone, Debug, PartialEq)]
pub struct ApiVersionsRequest<'a> {
    pub header: RequestHeader<'a>,
}

impl<'a> Encodable for ApiVersionsRequest<'a> {
    fn encode<T: ByteOrder>(self, dst: &mut BytesMut) -> Result<()> {
        self.header.encode::<T>(dst)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApiVersionsResponse {
    pub header: ResponseHeader,
    /// Error code.
    pub error_code: ErrorCode,
    /// API versions supported by the broker.
    pub api_versions: Vec<SupportedApiVersion>,
}

/// API versions supported by the broker.
#[derive(Clone, Debug, PartialEq)]
pub struct SupportedApiVersion {
    /// API key.
    pub api_key: ApiKey,
    /// Minimum supported version.
    pub min_version: ApiVersion,
    /// Maximum supported version.
    pub max_version: ApiVersion,
}

#[derive(Clone, Debug)]
pub struct SupportedApiVersions(Vec<SupportedApiVersion>);

impl SupportedApiVersions {
    pub fn find(&self, api_key: ApiKeys) -> Option<&SupportedApiVersion> {
        self.0.iter().find(|v| v.api_key == api_key as ApiKey)
    }
}

named!(pub parse_api_versions_response<ApiVersionsResponse>,
    parse_tag!(ParseTag::ApiVersionsResponse,
        do_parse!(
            header: parse_response_header
         >> error_code: be_i16
         >> api_versions: length_count!(be_i32, parse_api_version)
         >> (ApiVersionsResponse {
                header: header,
                error_code: error_code,
                api_versions: api_versions,
            })
        )
    )
);

named!(parse_api_version<SupportedApiVersion>,
    parse_tag!(ParseTag::ApiVersion,
        do_parse!(
            api_key: be_i16
         >> min_version: be_i16
         >> max_version: be_i16
         >> (SupportedApiVersion {
                api_key: api_key,
                min_version: min_version,
                max_version: max_version,
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
            api_versions: vec![SupportedApiVersion {
                api_key: 1,
                min_version: 2,
                max_version: 3,
            }],
        };
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

        assert_eq!(&buf[..], &TEST_REQUEST_DATA[..]);
    }

    #[test]
    fn test_parse_api_versions_response() {
        assert_eq!(parse_api_versions_response(TEST_RESPONSE_DATA.as_slice()),
        IResult::Done(&[][..], TEST_RESPONSE.clone()));
    }
}