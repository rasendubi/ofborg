extern crate amqp;
extern crate env_logger;

use lru_cache::LruCache;
use serde_json;
use std::fs;
use std::fs::{OpenOptions, File};
use std::path::{Component, PathBuf};
use std::io::Write;

use ofborg::writetoline::LineWriter;
use ofborg::message::buildlogmsg::{BuildLogStart, BuildLogMsg};
use ofborg::message::buildresult::BuildResult;
use ofborg::worker;
use amqp::protocol::basic::{Deliver, BasicProperties};

#[derive(Eq, PartialEq, Hash, Debug, Clone)]
pub struct LogFrom {
    routing_key: String,
    attempt_id: String,
}

pub struct LogMessageCollector {
    handles: LruCache<LogFrom, LineWriter>,
    log_root: PathBuf,
}

#[derive(Debug)]
enum MsgType {
    Start(BuildLogStart),
    Msg(BuildLogMsg),
    Finish(BuildResult),
}

#[derive(Debug)]
pub struct LogMessage {
    from: LogFrom,
    message: MsgType
}

fn validate_path_segment(segment: &PathBuf) -> Result<(), String> {
    let components = segment.components();

    if components.count() == 0 {
        return Err(String::from("Segment has no components"));
    }

    if segment.components().all(|component| match component {
        Component::Normal(_) => true,
        e => {
            println!("Invalid path component: {:?}", e);
            false
        }
    })
    {
        return Ok(());
    } else {
        return Err(String::from("Path contained invalid components"));
    }
}

impl LogMessageCollector {
    pub fn new(log_root: PathBuf, max_open: usize) -> LogMessageCollector {
        return LogMessageCollector {
            handles: LruCache::new(max_open),
            log_root: log_root,
        };
    }

    pub fn write_metadata(&mut self, from: &LogFrom, data: &BuildLogStart) -> Result<(), String>{
        let metapath = self.path_for_metadata(&from)?;
        let mut fp = self.open_file(metapath)?;

        match serde_json::to_string(data) {
            Ok(data) => {
                if let Err(e) = fp.write(&data.as_bytes()) {
                    Err(format!("Failed to write metadata: {:?}", e))
                } else {
                    Ok(())
                }
            },
            Err(e) => {
                Err(format!("Failed to stringify metadata: {:?}", e))
            }
        }
    }

    pub fn handle_for(&mut self, from: &LogFrom) -> Result<&mut LineWriter, String> {
        if self.handles.contains_key(&from) {
            return Ok(self.handles.get_mut(&from).expect(
                "handles just contained the key",
            ));
        } else {
            let logpath = self.path_for_log(&from)?;
            let fp = self.open_file(logpath)?;
            let writer = LineWriter::new(fp);
            self.handles.insert(from.clone(), writer);
            if let Some(handle) = self.handles.get_mut(&from) {
                return Ok(handle);
            } else {
                return Err(String::from(
                    "A just-inserted value should already be there",
                ));
            }
        }
    }

    fn path_for_metadata(&self, from: &LogFrom) -> Result<PathBuf, String> {
        let mut path = self.path_for_log(from)?;
        path.set_extension("metadata.json");
        return Ok(path);
    }

    fn path_for_log(&self, from: &LogFrom) -> Result<PathBuf, String> {
        let mut location = self.log_root.clone();

        let routing_key = PathBuf::from(from.routing_key.clone());
        validate_path_segment(&routing_key)?;
        location.push(routing_key);

        let attempt_id = PathBuf::from(from.attempt_id.clone());
        validate_path_segment(&attempt_id)?;
        location.push(attempt_id);

        if location.starts_with(&self.log_root) {
            return Ok(location);
        } else {
            return Err(format!(
                "Calculating the log location for {:?} resulted in an invalid path {:?}",
                from,
                location
            ));
        }
    }

    fn open_file(&self, path: PathBuf) -> Result<File, String> {
        let dir = path.parent().unwrap();
        fs::create_dir_all(dir).unwrap();

        let attempt = OpenOptions::new()
            .append(true)
            .read(true)
            .write(true)
            .create(true)
            .open(&path);

        match attempt {
            Ok(handle) => Ok(handle),
            Err(e) => Err(format!(
                "Failed to open the file for {:?}, err: {:?}",
                &path,
                e
            )),
        }
    }
}

impl worker::SimpleWorker for LogMessageCollector {
    type J = LogMessage;

    fn msg_to_job(
        &mut self,
        deliver: &Deliver,
        _: &BasicProperties,
        body: &Vec<u8>,
    ) -> Result<Self::J, String> {

        let message: MsgType;
        let attempt_id: String;

        let decode_msg: Result<BuildLogMsg, _> = serde_json::from_slice(body);
        if let Ok(msg) = decode_msg {
            attempt_id = msg.attempt_id.clone();
            message = MsgType::Msg(msg);
        } else {
            let decode_msg: Result<BuildLogStart, _> = serde_json::from_slice(body);
            if let Ok(msg) = decode_msg {
                attempt_id = msg.attempt_id.clone();
                message = MsgType::Start(msg);
            } else {
                let decode_msg: Result<BuildResult, _> = serde_json::from_slice(body);
                if let Ok(msg) = decode_msg {
                    attempt_id = msg.attempt_id.clone();
                    message = MsgType::Finish(msg);
                } else {
                    return Err(format!("failed to decode job: {:?}", decode_msg));
                }
            }
        }

        return Ok(LogMessage {
            from: LogFrom {
                routing_key: deliver.routing_key.clone(),
                attempt_id: attempt_id,
            },
            message: message
        });
    }

    fn consumer(&mut self, job: &LogMessage) -> worker::Actions {
        match job.message {
            MsgType::Start(ref start) => {
                self.write_metadata(&job.from, &start).expect("failed to write metadata");
            },
            MsgType::Msg(ref message) => {
                let handle = self.handle_for(&job.from).unwrap();

                handle.write_to_line((message.line_number - 1) as usize,
                                     &message.output);
            },
            MsgType::Finish(ref _finish) => {
            },
        }

        return vec![worker::Action::Ack];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::path::PathBuf;
    use ofborg::worker::SimpleWorker;
    use ofborg::test_scratch::TestScratch;

    fn make_worker(path: PathBuf) -> LogMessageCollector {
        LogMessageCollector::new(path, 3)
    }

    fn make_from(id: &str) -> LogFrom {
        LogFrom {
            attempt_id: format!("attempt-id-{}", &id),
            routing_key: format!("routing-key-{}", &id),
        }
    }

    #[test]
    fn test_handle_for() {
        let p = TestScratch::new_dir("log-message-collector-handle_for");

        let a = make_from("a.foo/123");
        let b = make_from("b.foo/123");
        let c = make_from("c.foo/123");
        let d = make_from("d.foo/123");

        let mut worker = make_worker(p.path());
        assert!(worker.handle_for(&a).is_ok());
        assert!(worker.handle_for(&b).is_ok());
        assert!(worker.handle_for(&c).is_ok());
        assert!(worker.handle_for(&d).is_ok());
        assert!(worker.handle_for(&a).is_ok());
    }

    #[test]
    fn test_path_for_metadata() {
        let p = TestScratch::new_dir("log-message-collector-path_for_metadata");
        let worker = make_worker(p.path());

        let path = worker
            .path_for_metadata(&LogFrom {
                attempt_id: String::from("my-attempt-id"),
                routing_key: String::from("my-routing-key"),
            })
            .expect("the path should be valid");


        assert!(path.starts_with(p.path()));
        assert!(path.as_os_str().to_string_lossy().ends_with("my-routing-key/my-attempt-id.metadata.json"));
    }

    #[test]
    fn test_path_for_log() {
        let p = TestScratch::new_dir("log-message-collector-path_for_log");
        let worker = make_worker(p.path());

        let path = worker
            .path_for_log(&LogFrom {
                attempt_id: String::from("my-attempt-id"),
                routing_key: String::from("my-routing-key"),
            })
            .expect("the path should be valid");


        assert!(path.starts_with(p.path()));
        assert!(path.ends_with("my-routing-key/my-attempt-id"));
    }

    #[test]
    fn test_path_for_log_malicious() {
        let p = TestScratch::new_dir("log-message-collector-for_malicious");
        let worker = make_worker(p.path());

        let path = worker.path_for_log(&LogFrom {
            attempt_id: String::from("./../../"),
            routing_key: String::from("./../../foobar"),
        });

        println!("path: {:?}", path);
        assert!(path.is_err());
    }

    #[test]
    fn test_validate_path_segment() {
        assert!(validate_path_segment(&PathBuf::from("foo")).is_ok());
        assert!(validate_path_segment(&PathBuf::from("foo/bar")).is_ok());
        assert!(validate_path_segment(&PathBuf::from("foo.bar/123")).is_ok());
        assert!(validate_path_segment(&PathBuf::from("..")).is_err());
        assert!(validate_path_segment(&PathBuf::from(".")).is_err());
        assert!(validate_path_segment(&PathBuf::from("./././")).is_err());
        assert!(validate_path_segment(&PathBuf::from("")).is_err());
        assert!(validate_path_segment(&PathBuf::from("foo/..")).is_err());
        assert!(validate_path_segment(&PathBuf::from("foo/../bar")).is_err());
        assert!(validate_path_segment(&PathBuf::from("foo/./bar")).is_ok());
        assert!(validate_path_segment(&PathBuf::from("/foo/bar")).is_err());
        assert!(validate_path_segment(&PathBuf::from("/foo")).is_err());
    }


    #[test]
    fn test_open_file() {
        let p = TestScratch::new_dir("log-message-collector-open_file");
        let worker = make_worker(p.path());

        assert!(
            worker
                .open_file(worker.path_for_log(&make_from("a")).unwrap())
                .is_ok()
        );
        assert!(
            worker
                .open_file(worker.path_for_log(&make_from("b.foo/123")).unwrap())
                .is_ok()
        );
    }

    #[test]
    pub fn test_logs_collect() {
        let mut logmsg = BuildLogMsg {
            attempt_id: String::from("my-attempt-id"),
            identity: String::from("my-identity"),
            system: String::from("foobar-x8664"),
            line_number: 1,
            output: String::from("line-1"),
        };
        let mut job = LogMessage {
            from: make_from("foo"),
            message: MsgType::Msg(logmsg.clone()),
        };

        let p = TestScratch::new_dir("log-message-collector-path_for_log");

        {
            let mut worker = make_worker(p.path());
            assert_eq!(vec![worker::Action::Ack],
                       worker.consumer(&
                                       LogMessage {
                                           from: make_from("foo"),
                                           message: MsgType::Start(BuildLogStart {
                                               attempt_id: String::from("my-attempt-id"),
                                               identity: String::from("my-identity"),
                                               system: String::from("foobar-x8664"),
                                               attempted_attrs: Some(vec!["foo".to_owned()]),
                                               skipped_attrs: Some(vec!["bar".to_owned()]),
                                           })
                                       }
                       )
            );

            assert_eq!(vec![worker::Action::Ack], worker.consumer(&job));

            logmsg.line_number = 5;
            logmsg.output = String::from("line-5");
            job.message = MsgType::Msg(logmsg.clone());
            assert_eq!(vec![worker::Action::Ack], worker.consumer(&job));

            job.from.attempt_id = String::from("my-other-attempt");
            logmsg.attempt_id = String::from("my-other-attempt");
            logmsg.line_number = 3;
            logmsg.output = String::from("line-3");
            job.message = MsgType::Msg(logmsg.clone());
            assert_eq!(vec![worker::Action::Ack], worker.consumer(&job));
        }

        let mut pr = p.path();
        let mut s = String::new();
        pr.push("routing-key-foo/attempt-id-foo.metadata.json");
        File::open(pr).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(&s, "{\"system\":\"foobar-x8664\",\"identity\":\"my-identity\",\"attempt_id\":\"my-attempt-id\",\"attempted_attrs\":[\"foo\"],\"skipped_attrs\":[\"bar\"]}");


        let mut pr = p.path();
        let mut s = String::new();
        pr.push("routing-key-foo/attempt-id-foo");
        File::open(pr).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(&s, "line-1\n\n\n\nline-5\n");


        let mut pr = p.path();
        let mut s = String::new();
        pr.push("routing-key-foo/my-other-attempt");
        File::open(pr).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(&s, "\n\nline-3\n");
    }
}
