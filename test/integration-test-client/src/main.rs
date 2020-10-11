use crate::http_client::{GetRequestTest, http_get, http_get_with_header_chunked, ChunkPattern, ConnAddr, GetRequest};
use std::time::Duration;
use crate::http_client::ClientHeader::{AutoGenerated, Custom};
use std::ops::Range;

mod http_client;

const DEFAULT_PORT: u16 = 7878;

struct PathGenerator {
    range: Range<i32>,
}
impl PathGenerator {
    fn generate(&mut self) -> String {
        format!("/test_{}", self.range.next().unwrap())
    }
}

fn main() {
    let mut path_generator = PathGenerator {
        range: 0..1000,
    };
    flexo_test_malformed_header();
    println!("flexo_test_malformed_header:              [SUCCESS]");
    flexo_test_partial_header(&mut path_generator);
    println!("flexo_test_partial_header:                [SUCCESS]");
    flexo_test_persistent_connections_c2s(&mut path_generator);
    println!("flexo_test_persistent_connections_c2s:    [SUCCESS]");
    flexo_test_persistent_connections_s2s(&mut path_generator);
    println!("flexo_test_persistent_connections_s2s:    [SUCCESS]");
    flexo_test_mirror_selection_slow_mirror(&mut path_generator);
    println!("flexo_test_mirror_selection_slow_mirror:  [SUCCESS]");
    flexo_test_download_large_file();
    println!("flexo_test_download_large_file:           [SUCCESS]");
}

fn flexo_test_malformed_header() {
    let malformed_header = "this is not a valid http header".to_owned();
    let uri1 = GetRequestTest {
        conn_addr: ConnAddr {
            host: "flexo-server".to_owned(),
            port: DEFAULT_PORT,
        },
        get_requests: vec![GetRequest {
            path: "/".to_owned(),
            client_header: Custom(malformed_header),
        }],
        timeout: None,
    };
    let results = http_get(uri1);
    assert_eq!(results.len(), 1);
    let result = results.get(0).unwrap();
    println!("result: {:?}", &result);
    assert_eq!(result.header_result.status_code, 400);
    // Test if the server is still up, i.e., the previous request hasn't crashed it:
    let uri2 = GetRequestTest {
        conn_addr: ConnAddr {
            host: "flexo-server".to_owned(),
            port: DEFAULT_PORT,
        },
        get_requests: vec![GetRequest {
            path: "/status".to_owned(),
            client_header: AutoGenerated,
        }],
        timeout: None,
    };
    let results = http_get(uri2);
    assert_eq!(results.len(), 1);
    let result = results.get(0).unwrap();
    println!("result: {:?}", &result);
    assert_eq!(result.header_result.status_code, 200);
}

fn flexo_test_partial_header(path_generator: &mut PathGenerator) {
    // Sending the header in multiple TCP segments does not cause the server to crash
    let uri = GetRequestTest {
        conn_addr: ConnAddr {
            host: "flexo-server-slow-primary".to_owned(),
            port: DEFAULT_PORT,
        },
        get_requests: vec![GetRequest {
            path: path_generator.generate(),
            client_header: AutoGenerated,
        }],
        timeout: None,
    };
    let pattern = ChunkPattern {
        chunk_size: 3,
        wait_interval: Duration::from_millis(300),
    };
    let results = http_get_with_header_chunked(uri, Some(pattern));
    assert_eq!(results.len(), 1);
    let result = results.get(0).unwrap();
    assert_eq!(result.header_result.status_code, 200);
}


fn flexo_test_persistent_connections_c2s(path_generator: &mut PathGenerator) {
    let request_test = GetRequestTest {
        conn_addr: ConnAddr {
            host: "flexo-server-delay".to_owned(),
            port: DEFAULT_PORT,
        },
        get_requests: vec![
            GetRequest {
                path: path_generator.generate(),
                client_header: AutoGenerated
            },
            GetRequest {
                path: path_generator.generate(),
                client_header: AutoGenerated
            },
            GetRequest {
                path: path_generator.generate(),
                client_header: AutoGenerated
            },
        ],
        timeout: None,
    };
    let results = http_get(request_test);
    assert_eq!(results.len(), 3);
    let all_ok = results.iter().all(|r| r.header_result.status_code == 200);
    assert!(all_ok);
}

fn flexo_test_persistent_connections_s2s(path_generator: &mut PathGenerator) {
    // Connections made from server-to-server (i.e., from flexo to the remote mirror) should be persistent.
    // We can test this only in an indirect manner: Based on the assumption that a short delay happens before
    // the flexo server can connect to the remote mirror, we conclude that if many files have been successfully
    // downloaded within the timeout, only one connection was established between the flexo server and the remote
    // mirror: If a new connection had been used for every request, the timeout would not have been sufficient.
    let get_requests: Vec<GetRequest> = (0..100).map(|_| {
        GetRequest {
            path: path_generator.generate(),
            client_header: AutoGenerated,
        }
    }).collect();
    let request_test = GetRequestTest {
        conn_addr: ConnAddr {
            host: "flexo-server-delay-primary".to_owned(),
            port: DEFAULT_PORT,
        },
        get_requests,
        timeout: Some(Duration::from_secs(1)),
    };
    let results = http_get(request_test);
    assert_eq!(results.len(), 100);
    let all_ok = results.iter().all(|r| r.header_result.status_code == 200);
    assert!(all_ok);
}

fn flexo_test_mirror_selection_slow_mirror(path_generator: &mut PathGenerator) {
    let get_requests = vec![
        GetRequest {
            path: path_generator.generate(),
            client_header: AutoGenerated,
        }
    ];
    let request_test = GetRequestTest {
        conn_addr: ConnAddr {
            host: "flexo-server-slow-primary".to_owned(),
            port: DEFAULT_PORT,
        },
        get_requests,
        timeout: Some(Duration::from_millis(500)),
    };
    let results = http_get(request_test);
    assert_eq!(results.len(), 1);
    let result = results.get(0).unwrap();
    assert_eq!(result.header_result.status_code, 200);
}

fn flexo_test_download_large_file() {
    // This test case is mainly used to provoke errors due to various 2GiB or 4GiB limits. For instance,
    // sendfile uses off_t as offset (see man 2 sendfile). off_t can be only 32 bit on some platforms.
    let get_requests = vec![
        GetRequest {
            path: "/zero".to_owned(),
            client_header: AutoGenerated,
        }
    ];
    let request_test = GetRequestTest {
        conn_addr: ConnAddr {
            host: "flexo-server-fast".to_owned(),
            port: DEFAULT_PORT,
        },
        get_requests,
        timeout: Some(Duration::from_millis(60_000)),
    };
    let results = http_get(request_test);
    assert_eq!(results.len(), 1);
    let result = results.get(0).unwrap();
    assert_eq!(result.header_result.status_code, 200);
    assert_eq!(result.payload_result.as_ref().unwrap().size, 8192 * 1024 * 1024)
}