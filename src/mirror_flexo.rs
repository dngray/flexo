extern crate flexo;

use crate::mirror_config::{MirrorsAutoConfig, MirrorConfig};
use crate::mirror_fetch;
use crate::mirror_fetch::MirrorUrl;

use flexo::*;
use http::Uri;
use std::fs::File;
use std::time::Duration;
use std::cmp::Ordering;
use crossbeam::crossbeam_channel::Sender;
use curl::easy::{Easy2, Handler, WriteError};
use std::fs::OpenOptions;
use std::io::{BufWriter, Error};
use std::io::prelude::*;
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use walkdir::WalkDir;
use xattr;
use std::ffi::OsString;
use std::net::{TcpListener, Shutdown, TcpStream};
use httparse::{Status, Header};
use std::io::{Read, ErrorKind, Write};
use std::path::{Path, PathBuf};

// Since a restriction for the size of header fields is also implemented by web servers like NGINX or Apache,
// we keep things simple by just setting a fixed buffer length.
// TODO return 414 (Request-URI Too Large) if this size is exceeded, instead of just panicking.
const MAX_HEADER_SIZE: usize = 8192;

const MAX_HEADER_COUNT: usize = 64;

#[cfg(test)]
pub const PATH_PREFIX: &str = "./";

#[cfg(not (test))]
pub const PATH_PREFIX: &str = "./";

#[cfg(test)]
const TEST_CHUNK_SIZE: usize = 128;

#[cfg(test)]
const TEST_REQUEST_HEADER: &[u8] = "GET / HTTP/1.1\r\nHost: www.example.com\r\n\r\n".as_bytes();

#[derive(Debug, PartialEq, Eq)]
pub enum StreamReadError {
    BufferSizeExceeded,
    TimedOut,
    SocketClosed,
    Other(ErrorKind)
}

#[derive(Debug, PartialEq, Eq)]
pub struct GetRequest {
    pub path: PathBuf,
}

impl GetRequest {
    fn new(request: httparse::Request) -> Self {
        match request.method {
            Some("GET") => {},
            method => panic!("Unexpected method: #{:?}", method)
        }
        let p = &request.path.unwrap()[1..]; // Skip the leading "/"
        let path = Path::new(p).to_path_buf();
        Self {
            path,
        }
    }
}

pub static DIRECTORY: &str = "./curl_ex_out/";

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct DownloadProvider {
    pub uri: Uri,
    pub mirror_results: MirrorResults,
    pub country: String,
}

impl Provider for DownloadProvider {
    type J = DownloadJob;

    fn new_job(&self, order: DownloadOrder) -> DownloadJob {
        let uri_string = format!("{}/{}", self.uri, order.filepath);
        let uri = uri_string.parse::<Uri>().unwrap();
        let provider = self.clone();
        DownloadJob {
            provider,
            uri,
            order,
        }
    }

    fn identifier(&self) -> &Uri {
        &self.uri
    }

    fn score(&self) -> MirrorResults {
        self.mirror_results
    }
}

#[derive(PartialEq, Eq, Hash, Copy, Clone, Debug, Default)]
pub struct MirrorResults {
    pub namelookup_duration: Duration,
    pub connect_duration: Duration,
}

impl Ord for MirrorResults {
    fn cmp(&self, other: &Self) -> Ordering {
        self.connect_duration.cmp(&other.connect_duration)
    }
}

impl PartialOrd for MirrorResults {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug)]
pub enum DownloadJobError {
    CurlError(curl::Error),
    HttpFailureStatus(u32),
}

#[derive(Debug)]
pub struct DownloadJob {
    provider: DownloadProvider,
    uri: Uri,
    order: DownloadOrder,
}

impl Job for DownloadJob {
    type S = MirrorResults;
    type JS = FileState;
    type C = DownloadChannel;
    type O = DownloadOrder;
    type P = DownloadProvider;
    type E = DownloadJobError;
    type CS = DownloadChannelState;
    type PI = Uri;
    type PR = MirrorsAutoConfig;
    type CH = TcpStream;

    fn provider(&self) -> &DownloadProvider {
        &self.provider
    }

    fn order(&self) -> DownloadOrder {
        self.order.clone()
    }

    fn initialize_cache() -> HashMap<DownloadOrder, u64, RandomState> {
        let mut hashmap: HashMap<Self::O, u64> = HashMap::new();
        for entry in WalkDir::new(DIRECTORY) {
            let entry = entry.expect("Error while reading directory entry");
            let key = OsString::from("user.content_length");
            if entry.file_type().is_file() {
                let file = File::open(entry.path()).expect(&format!("Unable to open file {:?}", entry.path()));
                let file_size = file.metadata().expect("Unable to fetch file metadata").len();
                let value = file_size.to_string();
                println!("File: {:?}", entry.path());
                match xattr::get(entry.path(), &key).expect("Unable to get extended file attributes") {
                    Some(_) => {},
                    None => {
                        // Flexo sets the extended attributes for all files, but this file lacks this attribute:
                        // We assume that this mostly happens when the user copies files into the directory used
                        // by flexo, and we further assume that users will do this only if this file is complete.
                        // Therefore, we can set the content length attribute of this file to the file size.
                        println!("Set content length for file: #{:?}", entry.path());
                        xattr::set(entry.path(), &key, &value.as_bytes())
                            .expect("Unable to set extended file attributes");
                    },
                }
                let order = DownloadOrder {
                    filepath: entry.path().to_str().unwrap().to_owned()
                };
                hashmap.insert(order, file_size);
            }
        }

        return hashmap;
    }

    fn serve_from_provider(self, mut channel: DownloadChannel, properties: MirrorsAutoConfig) -> JobResult<DownloadJob> {
        let url = format!("{}", &self.uri);
        channel.handle.url(&url).unwrap();
        // Limit the speed to facilitate debugging.
        // TODO disable the speed limit before releasing this.
        match properties.low_speed_limit {
            None => {},
            Some(speed) => {
                channel.handle.low_speed_limit(speed).unwrap();
                let low_speed_time_secs = properties.low_speed_time_secs.unwrap_or(4);
                channel.handle.low_speed_time(std::time::Duration::from_secs(low_speed_time_secs)).unwrap();
            },
        }
        match properties.max_speed_limit {
            None => {},
            Some(speed) => {
                channel.handle.max_recv_speed(speed).unwrap();
            },
        }
        channel.handle.follow_location(true).unwrap();
        channel.handle.max_redirections(3).unwrap();
        match channel.progress_indicator() {
            None => {},
            Some(start) => {
                channel.handle.resume_from(start).unwrap();
            }
        }
        match channel.handle.perform() {
            Ok(()) => {
                let response_code = channel.handle.response_code().unwrap();
                if response_code >= 200 && response_code < 300 {
                    println!("Success!");
                    JobResult::Complete(JobCompleted::new(channel, self.provider))
                } else {
                    let termination = JobTerminated {
                        channel,
                        error: DownloadJobError::HttpFailureStatus(response_code),
                    };
                    JobResult::Error(termination)
                }
            },
            Err(e) => {
                match channel.progress_indicator() {
                    Some(size) if size > 0 => {
                        JobResult::Partial(JobPartiallyCompleted::new(channel, size))
                    }
                    _ => {
                        let termination = JobTerminated {
                            channel,
                            error: DownloadJobError::CurlError(e),
                        };
                        JobResult::Error(termination)
                    }
                }
            }
        }
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct DownloadOrder {
    /// This path is relative to the given root directory.
    pub filepath: String,
}

impl Order for DownloadOrder {
    type J = DownloadJob;

    fn new_channel(self, tx: Sender<FlexoProgress>) -> DownloadChannel {
        DownloadChannel {
            handle: Easy2::new(DownloadState::new(self, tx).unwrap()),
            state: DownloadChannelState::new(),
        }
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Debug, Copy)]
pub struct DownloadChannelState {
    is_reset: bool,
}

impl DownloadChannelState {
    fn new() -> Self {
        Self {
            is_reset: false
        }
    }
}

impl ChannelState for DownloadChannelState {
    type J = DownloadJob;

    fn reset(&mut self) {
        self.is_reset = true;
    }
}

#[derive(Debug)]
pub struct FileState {
    buf_writer: BufWriter<File>,
    size_written: u64,
}

impl JobState for FileState {
    type J = DownloadJob;
}

#[derive(Debug)]
struct DownloadState {
    job_state: JobStateItem<DownloadJob>
}

impl DownloadState {
    pub fn new(order: DownloadOrder, tx: Sender<FlexoProgress>) -> std::io::Result<Self> {
        let path = DIRECTORY.to_owned() + &order.filepath;
        println!("Attempt to create file: {:?}", path);
        let f = OpenOptions::new().create(true).append(true).open(path)?;
        let size_written = f.metadata()?.len();
        let buf_writer = BufWriter::new(f);
        let job_state = JobStateItem {
            order,
            state: Some(FileState {
                buf_writer,
                size_written,
            }),
            tx,
        };
        Ok(DownloadState { job_state })
    }

    pub fn reset(&mut self, order: DownloadOrder, tx: Sender<FlexoProgress>) -> std::io::Result<()> {
        if order != self.job_state.order {
            let c = DownloadState::new(order.clone(), tx)?;
            self.job_state.state = c.job_state.state;
            self.job_state.order = order;
        }
        Ok(())
    }
}

impl Handler for DownloadState {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        match self.job_state.state.iter_mut().next() {
            None => panic!("Expected the state to be initialized."),
            Some(file_state) => {
                file_state.size_written += data.len() as u64;
                match file_state.buf_writer.write(data) {
                    Ok(size) => {
                        let len = file_state.buf_writer.get_ref().metadata().unwrap().len();
                        let _result = self.job_state.tx.send(FlexoProgress::Progress(len));
                        Ok(size)
                    },
                    Err(e) => {
                        println!("Error while writing data: {:?}", e);
                        Err(WriteError::Pause)
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct DownloadChannel {
    handle: Easy2<DownloadState>,
    state: DownloadChannelState,
}

impl Channel for DownloadChannel {
    type J = DownloadJob;

    fn progress_indicator(&self) -> Option<u64> {
        let file_state = self.handle.get_ref().job_state.state.as_ref().unwrap();
        let size_written = file_state.size_written;
        if size_written > 0 {
            Some(size_written)
        } else {
            None
        }
    }

    fn reset_order(&mut self, order: DownloadOrder, tx: Sender<FlexoProgress>) {
        self.handle.get_mut().reset(order, tx).unwrap();
    }

    fn channel_state_item(&mut self) -> &mut JobStateItem<DownloadJob> {
        &mut self.handle.get_mut().job_state
    }

    fn channel_state(&self) -> DownloadChannelState {
        self.state
    }

    fn channel_state_ref(&mut self) -> &mut DownloadChannelState {
        &mut self.state
    }
}

pub fn rate_providers(mut mirror_urls: Vec<MirrorUrl>, mirror_config: &MirrorConfig) -> Vec<DownloadProvider> {
    mirror_urls.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap());
    let filtered_mirror_urls: Vec<MirrorUrl> = mirror_urls
        .into_iter()
        .filter(|x| x.filter_predicate(&mirror_config))
        .take(mirror_config.mirrors_auto.num_mirrors)
        .collect();
    let mut mirrors_with_latencies = Vec::new();
    let timeout = Duration::from_millis(mirror_config.mirrors_auto.timeout);
    for mirror in filtered_mirror_urls.into_iter() {
        match mirror_fetch::measure_latency(&mirror.url, timeout) {
            None => {},
            Some(latency) => {
                mirrors_with_latencies.push((mirror, latency));
            }
        }
    }
    mirrors_with_latencies.sort_unstable_by_key(|(_, latency)| {
        *latency
    });

    mirrors_with_latencies.into_iter().map(|(mirror, mirror_results)| {
        DownloadProvider {
            uri: mirror.url.parse::<Uri>().unwrap(),
            mirror_results,
            country: mirror.country,
        }
    }).collect()
}

pub fn read_header<T>(stream: &mut T) -> Result<GetRequest, StreamReadError> where T: Read {
    let mut buf = [0; MAX_HEADER_SIZE + 1];
    let mut size_read_all = 0;

    loop {
        if size_read_all >= MAX_HEADER_SIZE {
            return Err(StreamReadError::BufferSizeExceeded);
        }
        let size = match stream.read(&mut buf[size_read_all..]) {
            Ok(s) if s > MAX_HEADER_SIZE => return Err(StreamReadError::BufferSizeExceeded),
            Ok(s) if s > 0 => s,
            Ok(_) => {
                // we need this branch in case the socket is closed: Otherwise, we would read a size of 0
                // indefinitely.
                return Err(StreamReadError::SocketClosed);
            }
            Err(e) => {
                let error = match e.kind() {
                    ErrorKind::TimedOut => StreamReadError::TimedOut,
                    ErrorKind::WouldBlock => StreamReadError::TimedOut,
                    other => StreamReadError::Other(other),
                };
                return Err(error);
            }
        };
        size_read_all += size;

        println!("size: {:?}", size);
        println!("buf: {:?}", &buf[0..31]);

        let mut headers: [Header; 64] = [httparse::EMPTY_HEADER; MAX_HEADER_COUNT];
        let mut req: httparse::Request = httparse::Request::new(&mut headers);
        let res: std::result::Result<httparse::Status<usize>, httparse::Error> = req.parse(&buf[..size_read_all]);

        match res {
            Ok(Status::Complete(result)) => {
                println!("done! {:?}", result);
                println!("req: {:?}", req);
                break(Ok(GetRequest::new(req)))
            }
            Ok(Status::Partial) => {
                println!("partial");
            }
            Err(e) => {
                println!("error: #{:?}", e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, Write};

    struct TooMuchDataReader {}
    impl Read for TooMuchDataReader {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
            // Notice that we cause an error by writing the exact amount of the maximum header size,
            // has received this exact amount of bytes, or if it has received more than 8192 bytes but the returned value
            // is 8192 because that is the maximum buffer size. So we cautiously assume the latter case and return an error.
            let too_much_data = [0; MAX_HEADER_SIZE + 1];
            buf[..too_much_data.len()].copy_from_slice(&too_much_data);
            Ok(MAX_HEADER_SIZE + 1)
        }
    }

    // writes a single byte at a time.
    struct OneByteReader {
        size_read: usize,
    }
    impl OneByteReader {
        fn new() -> Self {
            OneByteReader {
                size_read: 0,
            }
        }
    }
    impl Read for OneByteReader {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
            if self.size_read < TEST_REQUEST_HEADER.len() {
                buf[0] = TEST_REQUEST_HEADER[self.size_read];
                self.size_read += 1;
                Ok(1)
            } else {
                Ok(0)
            }
        }
    }
    struct NoDelimiterReader {
    }
    impl NoDelimiterReader {
        fn new() -> Self {
            NoDelimiterReader {}
        }
    }
    impl Read for NoDelimiterReader {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
            let array: [u8; TEST_CHUNK_SIZE] = [b'a'; TEST_CHUNK_SIZE];
            buf[0..TEST_CHUNK_SIZE].copy_from_slice(&array);
            Ok(256)
        }
    }

    #[test]
    fn test_buffer_size_exceeded() {
        let mut stream = TooMuchDataReader {};
        let result = read_header(&mut stream);
        assert_eq!(result, Err(StreamReadError::BufferSizeExceeded));
    }

    #[test]
    fn test_one_by_one() {
        let mut stream = OneByteReader::new();
        let result = read_header(&mut stream);
        println!("result: {:?}", result);
        assert!(result.is_ok());
    }

    #[test]
    fn test_no_delimiter() {
        let mut stream = NoDelimiterReader::new();
        let result = read_header(&mut stream);
        assert_eq!(result, Err(StreamReadError::BufferSizeExceeded));
    }
}