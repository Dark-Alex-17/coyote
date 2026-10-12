use crate::config::paths;
use colored::Colorize;
use fancy_regex::Regex;
use same_file::Handle;
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind, Seek, SeekFrom};
use std::path::Path;
use std::process;
use std::time::Duration;
use tokio::time::sleep;

pub async fn tail_logs(no_color: bool) {
    let re = Regex::new(r"^(?P<timestamp>\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3})\s+<(?P<opid>[^\s>]+)>\s+\[(?P<level>[A-Z]+)\]\s+(?P<logger>[^:]+):(?P<line>\d+)\s+-\s+(?P<message>.*)$").unwrap();
    let file_path = paths::log_file();
    let file = File::open(&file_path).expect("Cannot open file");
    let mut reader = BufReader::new(file);

    if let Err(e) = reader.seek(SeekFrom::End(0)) {
        eprintln!("Unable to tail log file: {e:?}");
        process::exit(1);
    };

    let mut line_buf = String::new();

    loop {
        match reader.read_line(&mut line_buf) {
            Ok(0) => {
                reopen_if_rotated(&file_path, &mut reader);
                sleep(Duration::from_millis(100)).await;
            }
            Ok(_) => {
                let line = line_buf.trim_end();
                if no_color {
                    println!("{line}");
                } else {
                    let colored_line = colorize_log_line(line, &re);
                    println!("{colored_line}");
                }
                line_buf.clear();
            }
            Err(_) => {
                line_buf.clear();
                sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

fn reopen_if_rotated(path: &Path, reader: &mut BufReader<File>) {
    if !file_was_rotated(path, reader) {
        return;
    }

    match File::open(path) {
        Ok(file) => *reader = BufReader::new(file),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => {
            eprintln!("Unable to reopen log file: {e}");
            process::exit(1);
        }
    }
}

fn file_was_rotated(path: &Path, reader: &mut BufReader<File>) -> bool {
    let current_pos = reader.stream_position().unwrap_or(0);
    let Ok(at_path) = File::open(path) else {
        return true;
    };

    if at_path.metadata().is_ok_and(|m| m.len() < current_pos) {
        return true;
    }

    let open = reader.get_ref().try_clone().and_then(Handle::from_file);
    match (Handle::from_file(at_path), open) {
        (Ok(at_path), Ok(open)) => at_path != open,
        _ => true,
    }
}

fn colorize_log_line(line: &str, re: &Regex) -> String {
    if let Some(caps) = re.captures(line).expect("Failed to capture log line") {
        let level = &caps["level"];
        let message = &caps["message"];

        let colored_message = match level {
            "ERROR" => message.red(),
            "WARN" => message.yellow(),
            "INFO" => message.green(),
            "DEBUG" => message.blue(),
            _ => message.normal(),
        };

        let timestamp = &caps["timestamp"];
        let opid = &caps["opid"];
        let logger = &caps["logger"];
        let line_number = &caps["line"];

        format!(
            "{} <{}> [{}] {}:{} - {}",
            timestamp.white(),
            opid.cyan(),
            level.bold(),
            logger.magenta(),
            line_number.bold(),
            colored_message
        )
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::temp_file;
    use std::fs;
    use std::io::{Read, Write};
    use std::path::PathBuf;

    fn reader_at_eof(dir: &Path) -> (PathBuf, BufReader<File>) {
        fs::create_dir_all(dir).unwrap();
        let log = dir.join("coyote.log");
        fs::write(&log, "0123456789").unwrap();
        let mut reader = BufReader::new(File::open(&log).unwrap());
        let mut sink = String::new();
        reader.read_to_string(&mut sink).unwrap();
        (log, reader)
    }

    fn reader_handle(reader: &BufReader<File>) -> Handle {
        Handle::from_file(reader.get_ref().try_clone().unwrap()).unwrap()
    }

    #[test]
    fn rotation_is_detected_when_new_file_is_longer_than_reader_position() {
        let dir = temp_file("-logs-rotated-", "");
        let (log, mut reader) = reader_at_eof(&dir);

        fs::rename(&log, dir.join("coyote.archived.1.log")).unwrap();
        fs::write(&log, "x".repeat(50)).unwrap();

        assert!(file_was_rotated(&log, &mut reader));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_rotation_when_same_file_grows() {
        let dir = temp_file("-logs-grows-", "");
        let (log, mut reader) = reader_at_eof(&dir);

        fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .unwrap()
            .write_all(b"more")
            .unwrap();

        assert!(!file_was_rotated(&log, &mut reader));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_in_place_counts_as_rotated() {
        let dir = temp_file("-logs-truncated-", "");
        let (log, mut reader) = reader_at_eof(&dir);

        File::create(&log).unwrap().set_len(0).unwrap();

        assert!(file_was_rotated(&log, &mut reader));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_path_counts_as_rotated() {
        let dir = temp_file("-logs-missing-", "");
        let (log, mut reader) = reader_at_eof(&dir);

        fs::remove_file(&log).unwrap();

        assert!(file_was_rotated(&log, &mut reader));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopen_keeps_old_reader_until_new_file_exists_then_swaps() {
        let dir = temp_file("-logs-reopen-", "");
        let (log, mut reader) = reader_at_eof(&dir);
        let archived = dir.join("coyote.archived.1.log");
        fs::rename(&log, &archived).unwrap();

        reopen_if_rotated(&log, &mut reader);

        assert_eq!(
            reader_handle(&reader),
            Handle::from_path(&archived).unwrap()
        );
        assert_eq!(reader.stream_position().unwrap(), 10);

        fs::write(&log, "fresh").unwrap();
        reopen_if_rotated(&log, &mut reader);

        assert_eq!(reader_handle(&reader), Handle::from_path(&log).unwrap());
        assert_eq!(reader.stream_position().unwrap(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
