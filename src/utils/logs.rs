use crate::config::Config;
use colored::Colorize;
use fancy_regex::Regex;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::process;

pub async fn tail_logs(no_color: bool) {
    let re = Regex::new(r"^(?P<timestamp>\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3})\s+<(?P<opid>[^\s>]+)>\s+\[(?P<level>[A-Z]+)\]\s+(?P<logger>[^:]+):(?P<line>\d+)\s+-\s+(?P<message>.*)$").unwrap();
    let file_path = Config::log_path();
    let file = File::open(&file_path).expect("Cannot open file");
    let mut reader = BufReader::new(file);

    if let Err(e) = reader.seek(SeekFrom::End(0)) {
        eprintln!("Unable to tail log file: {e:?}");
        process::exit(1);
    };

    let mut lines = reader.lines();

    loop {
        if let Some(Ok(line)) = lines.next() {
            if no_color {
                println!("{line}");
            } else {
                let colored_line = colorize_log_line(&line, &re);
                println!("{colored_line}");
            }
        }
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
