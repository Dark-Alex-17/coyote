use crate::config::paths;
use colored::Colorize;
use fancy_regex::Regex;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
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
                if file_was_rotated(&file_path, &mut reader) {
                    let file = File::open(&file_path).expect("Cannot open file");
                    reader = BufReader::new(file);
                }
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

fn file_was_rotated(path: &std::path::Path, reader: &mut BufReader<File>) -> bool {
    let current_pos = reader.stream_position().unwrap_or(0);
    match fs::metadata(path) {
        Ok(metadata) => metadata.len() < current_pos,
        Err(_) => true,
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
