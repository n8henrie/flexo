#[macro_use] extern crate log;

extern crate http;
extern crate rand;
extern crate flexo;

use std::io::prelude::*;
use std::sync::{Arc, Mutex};
use http::Uri;
use flexo::*;
use crate::mirror_config::{MirrorSelectionMethod, MirrorConfig};
use crossbeam::crossbeam_channel::Receiver;
use mirror_flexo::*;
use std::os::unix::io::AsRawFd;

mod mirror_config;
mod mirror_fetch;
mod mirror_cache;
mod mirror_flexo;

use std::net::{TcpListener, TcpStream, SocketAddr};
use std::time::Duration;
use std::path;
use std::path::Path;
use std::fs::File;
use crossbeam::crossbeam_channel::RecvTimeoutError;
use std::ffi::OsString;


#[cfg(test)]
use tempfile::tempfile;
use std::io::ErrorKind;

// man 2 read: read() (and similar system calls) will transfer at most 0x7ffff000 bytes.
#[cfg(not(test))]
const MAX_SENDFILE_COUNT: usize = 0x7fff_f000;

// Choose a smaller size in test, this makes it easier to have fast tests.
#[cfg(test)]
const MAX_SENDFILE_COUNT: usize = 128;

fn main() {
    env_logger::init();

    // Exit the entire process when a single thread panics:
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        hook(panic_info);
        std::process::exit(1);
    }));

    let properties = mirror_config::load_config();
    match properties.low_speed_limit {
        None => {},
        Some(limit) => {
            info!("Will switch mirror if download speed falls below {}/s", size_to_human_readable(limit.into()));
        },
    }
    let job_context: Arc<Mutex<JobContext<DownloadJob>>> = match initialize_job_context(properties.clone()) {
        Ok(jc) =>  Arc::new(Mutex::new(jc)),
        Err(ProviderSelectionError::NoProviders) => {
            error!("Unable to find remote mirrors that match the selected criteria.");
            std::process::exit(1);
        }
    };
    let port = job_context.lock().unwrap().properties.port;
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).unwrap();
    for stream in listener.incoming() {
        let stream: TcpStream = stream.unwrap();
        debug!("Established connection with client.");
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let job_context = job_context.clone();
        let properties = properties.clone();
        std::thread::spawn(move || {
            serve_client(job_context, stream, properties)
        });
    }
}

fn valid_path(path: &Path) -> bool {
    path.components().all(|c| {
        match c {
            path::Component::Normal(_) => true,
            path::Component::RootDir => true,
            _ => false,
        }
    })
}

fn serve_client(job_context: Arc<Mutex<JobContext<DownloadJob>>>, mut stream: TcpStream, properties: MirrorConfig) -> Result<(), ClientError> {
    // Loop for persistent connections: Will wait for subsequent requests instead of closing immediately.
    loop {
        debug!("Read header from client.");
        let result = read_client_header(&mut stream);
        match result {
            Ok(get_request) if !valid_path(&get_request.path) => {
                info!("Invalid path: Serve 403");
                serve_403_header(&mut stream);
            }
            Ok(get_request) if get_request.path.as_os_str() == "status" => {
                serve_200_ok_empty(&mut stream)
            }
            Ok(get_request) => {
                let path = Path::new(PATH_PREFIX).join(&get_request.path);
                let order = DownloadOrder {
                    filepath: path.to_str().unwrap().to_owned()
                };
                debug!("Attempt to schedule new job");
                let result = job_context.lock().unwrap().try_schedule(order.clone(), get_request.resume_from);
                match result {
                    ScheduleOutcome::AlreadyInProgress => {
                        debug!("Job is already in progress");
                        let path = Path::new(&properties.cache_directory).join(&order.filepath);
                        let complete_filesize: u64 = try_complete_filesize_from_path(&path).unwrap();
                        let content_length = complete_filesize - get_request.resume_from.unwrap_or(0);
                        let file: File = File::open(&path).unwrap();
                        serve_from_growing_file(file, content_length, get_request.resume_from, &mut stream);
                    }
                    ScheduleOutcome::Scheduled(ScheduledItem { rx_progress, .. }) => {
                        // TODO this branch is also executed when the server returns 404.
                        debug!("Job was scheduled, will serve from growing file");
                        match receive_content_length(rx_progress) {
                            Ok(content_length) => {
                                debug!("Received content length via channel: {}", content_length);
                                let path = Path::new(&properties.cache_directory).join(&order.filepath);
                                let file: File = File::open(&path).unwrap();
                                serve_from_growing_file(file, content_length, get_request.resume_from, &mut stream);
                            },
                            Err(ContentLengthError::Unavailable) => {
                                debug!("Will send 404 reply to client.");
                                serve_404_header(&mut stream);
                            },
                            Err(ContentLengthError::OrderError) => {
                                debug!("Will send 400 reply to client.");
                                serve_400_header(&mut stream);
                            },
                            Err(ContentLengthError::TransmissionError(RecvTimeoutError::Disconnected)) => {
                                eprintln!("Remote server has disconnected unexpectedly.");
                                serve_500_header(&mut stream);
                            },
                            Err(e) => {
                                panic!("Error: {:?}", e)
                            },
                        }
                    },
                    ScheduleOutcome::Cached => {
                        debug!("Serve file from cache.");
                        let path = Path::new(&properties.cache_directory).join(&order.filepath);
                        let file: File = File::open(path).unwrap();
                        serve_from_complete_file(file, get_request.resume_from, &mut stream);
                    },
                    ScheduleOutcome::Uncacheable(p) => {
                        debug!("Serve file via redirect.");
                        let uri_string = format!("{}{}", p.uri, order.filepath);
                        serve_via_redirect(uri_string, &mut stream);
                    }
                }
                info!("Request served: {:?}", &get_request.path);
            },
            Err(e) => {
                match e {
                    ClientError::SocketClosed => {
                        debug!("Socket closed by client.");
                    }
                    ClientError::Other(kind) if kind == ErrorKind::ConnectionReset => {
                        debug!("Socket closed by client.");
                    }
                    ClientError::TimedOut => {
                        debug!("Connection client-to-server has timed out. New connection required \
                        for subsequent requests from the client.");
                    }
                    ClientError::UnsupportedHttpMethod(ClientStatus { response_headers_sent }) => {
                        error!("The client has used an HTTP method that is not supported by flexo.");
                        if !response_headers_sent {
                            serve_400_header(&mut stream);
                        }
                    },
                    ClientError::InvalidHeader(ClientStatus { response_headers_sent }) => {
                        error!("The client has sent an invalid header");
                        if !response_headers_sent {
                            serve_400_header(&mut stream);
                        }
                    }
                    _ => {
                        eprintln!("error: {:?}", e);
                    }
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return Err(e);
            }
        };
    }
}

pub enum ProviderSelectionError {
    NoProviders,
}

fn initialize_job_context(properties: MirrorConfig) -> Result<JobContext<DownloadJob>, ProviderSelectionError> {
    let providers: Vec<DownloadProvider> = rated_providers(&properties);
    debug!("mirrors: {:#?}", providers);
    if providers.is_empty() {
        return Err(ProviderSelectionError::NoProviders)
    }
    info!("Primary mirror: {:#?}", providers[0].uri);
    let urls: Vec<String> = providers.iter().map(|x| x.uri.to_string()).collect();
    mirror_cache::store(&properties, &urls);

    // Change the implementation so that mirror_config is accepted.
    // We need mirror_config so that we can access the port, so that the user may modify the port via the TOML file.
    Ok(JobContext::new(providers, properties))
}

fn rated_providers(mirror_config: &MirrorConfig) -> Vec<DownloadProvider> {
    if mirror_config.mirror_selection_method == MirrorSelectionMethod::Auto {
        match mirror_fetch::fetch_providers_from_json_endpoint(mirror_config) {
            Ok(mirror_urls) => rate_providers(mirror_urls, &mirror_config),
            Err(e) => {
                info!("Unable to fetch mirrors remotely: {:?}", e);
                info!("Will try to fetch them from cache.");
                let mirrors = mirror_cache::fetch(&mirror_config).unwrap();
                mirrors.iter().map(|url| {
                    DownloadProvider {
                        uri: url.parse::<Uri>().unwrap(),
                        mirror_results: MirrorResults::default(),
                        country: "unknown".to_owned(),
                    }
                }).collect()
            },
        }
    } else {
        let default_mirror_result: MirrorResults = Default::default();
        let mirrors_predefined = mirror_config.mirrors_predefined.clone();
        mirrors_predefined.into_iter().map(|uri| {
            DownloadProvider {
                uri: uri.parse::<Uri>().unwrap(),
                mirror_results: default_mirror_result,
                country: "Unknown".to_owned(),
            }
        }).collect()
    }
}

#[derive(Debug)]
enum ContentLengthError {
    TransmissionError(RecvTimeoutError),
    Unavailable,
    OrderError,
}

fn receive_content_length(rx: Receiver<FlexoProgress>) -> Result<u64, ContentLengthError> {
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(FlexoProgress::JobSize(content_length)) => {
                break Ok(content_length);
            }
            Ok(FlexoProgress::Unavailable) => {
                break Err(ContentLengthError::Unavailable);
            }
            Ok(FlexoProgress::OrderError) => {
                break Err(ContentLengthError::OrderError);
            }
            Err(e) => break Err(ContentLengthError::TransmissionError(e)),
            Ok(msg) => {
                panic!("Unexpected message: {:?}", msg);
            },
        }
    }
}

/// Returns the size of the complete file. This size may be larger than the size we have stored locally.
fn try_complete_filesize_from_path(path: &Path) -> Option<u64> {
    let mut num_attempts = 0;
    // Timeout after 2 seconds.
    while num_attempts < 2_000 * 2 {
        match content_length_from_path(path) {
            None => {
                std::thread::sleep(std::time::Duration::from_micros(500));
            },
            Some(v) => return Some(v),
        }
        num_attempts += 1;
    }

    info!("Number of attempts exceeded: File {:?} not found.", &path);
    None
}

fn content_length_from_path(path: &Path) -> Option<u64> {
    let key = OsString::from("user.content_length");
    let value = xattr::get(&path, &key);
    match value {
        Ok(Some(value)) => {
            let content_length = String::from_utf8(value).unwrap().parse::<u64>().unwrap();
            debug!("Found file! content length is {}", content_length);
            Some(content_length)
        },
        Ok(None) => {
            info!("file exists, but no content length is set.");
            None
        }
        Err(_) => {
            None
        }
    }
}

fn serve_from_growing_file(mut file: File, content_length: u64, resume_from: Option<u64>, stream: &mut TcpStream) {
    let header = match resume_from {
        None => reply_header_success(content_length),
        Some(r) => reply_header_partial(content_length, r)
    };
    stream.write_all(header.as_bytes()).unwrap();
    let resume_from = resume_from.unwrap_or(0);
    let mut client_received = resume_from;
    let complete_filesize = content_length + resume_from;
    while client_received < complete_filesize {
        let filesize = file.metadata().unwrap().len();
        if filesize > client_received {
            // TODO note that this while loop runs indefinitely if the file stops growing for whatever reason.
            let result = send_payload(&mut file, filesize, client_received as i64, stream);
            match result {
                Ok(_) => {
                    client_received = result.unwrap() as u64;
                },
                Err(e) => {
                    if e.kind() == ErrorKind::BrokenPipe || e.kind() == ErrorKind::ConnectionReset {
                        debug!("Connection closed by client?");
                        return;
                    } else {
                        panic!("Unexpected error: {:?}", e);
                    }
                },
            }
        }
        if client_received < content_length {
            std::thread::sleep(std::time::Duration::from_micros(500));
        }
    }
    debug!("File completely served from growing file.");
}

fn serve_404_header(stream: &mut TcpStream) {
    let header = reply_header_not_found();
    stream.write_all(header.as_bytes()).unwrap();
    // TODO do we really need another "\r\n" here? same with all other serve_xxx_header functions.
    stream.write_all(b"\r\n").unwrap();
}

fn serve_400_header(stream: &mut TcpStream) {
    let header = reply_header_bad_request();
    stream.write_all(header.as_bytes()).unwrap();
    stream.write_all(b"\r\n").unwrap();
}

fn serve_500_header(stream: &mut TcpStream) {
    let header = reply_header_internal_server_error();
    stream.write_all(header.as_bytes()).unwrap();
    stream.write_all(b"\r\n").unwrap();
}

fn serve_403_header(stream: &mut TcpStream) {
    let header = reply_header_forbidden();
    stream.write_all(header.as_bytes()).unwrap();
    stream.write_all(b"\r\n").unwrap();
}

fn serve_200_ok_empty(stream: &mut TcpStream) {
    let header = reply_header_success(0);
    stream.write_all(header.as_bytes()).unwrap();
}

fn reply_header_success(content_length: u64) -> String {
    reply_header("200 OK", content_length, None)
}

fn reply_header_partial(content_length: u64, resume_from: u64) -> String {
    reply_header("206 Partial Content", content_length, Some(resume_from))
}

fn reply_header_not_found() -> String {
    reply_header("404 Not Found", 0, None)
}

fn reply_header_bad_request() -> String {
    reply_header("400 Bad Request", 0, None)
}

fn reply_header_internal_server_error() -> String {
    reply_header("500 Internal Server Error", 0, None)
}

fn reply_header_forbidden() -> String {
    reply_header("403 Forbidden", 0, None)
}

fn reply_header(status_line: &str, content_length: u64, resume_from: Option<u64>) -> String {
    let now = time::now_utc();
    let timestamp = now.rfc822();
    let content_range_header = resume_from.map(|r| {
        let complete_size = content_length + r;
        let last_byte = complete_size - 1;
        format!("Content-Range: bytes {}-{}/{}\r\n", r, last_byte, complete_size)
    }).unwrap_or_else(|| "".to_owned());
    let header = format!("\
        HTTP/1.1 {}\r\n\
        Server: flexo\r\n\
        Date: {}\r\n\
        {}\
        Content-Length: {}\r\n\r\n", status_line, timestamp, content_range_header, content_length);
    debug!("Sending header to client: {:?}", &header);

    header
}

fn redirect_header(path: &str) -> String {
    let now = time::now_utc();
    let timestamp = now.rfc822();
    let header = format!("\
        HTTP/1.1 301 Moved Permanently\r\n\
        Server: flexo\r\n\
        Date: {}\r\n\
        Content-Length: 0\r\n\
        Location: {}\r\n\r\n", timestamp, path);

    header
}

fn serve_from_complete_file(mut file: File, resume_from: Option<u64>, stream: &mut TcpStream) {
    let filesize = file.metadata().unwrap().len();
    let content_length = filesize - resume_from.unwrap_or(0);
    let header = match resume_from {
        None => reply_header_success(content_length),
        Some(r) => reply_header_partial(content_length, r)
    };
    stream.write_all(header.as_bytes()).unwrap();
    let bytes_sent = resume_from.unwrap_or(0) as i64;
    send_payload(&mut file, filesize, bytes_sent, stream).unwrap();
}

fn serve_via_redirect(uri: String, stream: &mut TcpStream) {
    let header = redirect_header(&uri);
    stream.write_all(header.as_bytes()).unwrap();
}

fn send_payload<T>(source: &mut File, filesize: u64, bytes_sent: i64, receiver: &mut T) -> Result<i64, std::io::Error> where T: AsRawFd {
    let fd = source.as_raw_fd();
    let sfd = receiver.as_raw_fd();
    let size = unsafe {
        let mut offset = bytes_sent;
        while (offset as u64) < filesize {
            let size: isize = libc::sendfile(sfd, fd, &mut offset, MAX_SENDFILE_COUNT);
            if size == -1 {
                return Err(std::io::Error::last_os_error());
            }
        }
        offset
    };
    Ok(size)
}

#[test]
fn test_filesize_exceeds_sendfile_count() {
    let mut source: File = tempfile().unwrap();
    let mut receiver: File = tempfile().unwrap();
    let array: [u8; MAX_SENDFILE_COUNT * 3] = [b'a'; MAX_SENDFILE_COUNT * 3];
    source.write(&array).unwrap();
    source.flush().unwrap();
    let filesize = source.metadata().unwrap().len();
    let size = send_payload(&mut source, filesize, 0, &mut receiver).unwrap();
    assert_eq!(size, (MAX_SENDFILE_COUNT * 3) as i64);
}
