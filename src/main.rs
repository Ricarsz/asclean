//! asclean — 浏览、筛选与删除本机 AI 编码客户端的会话。

#![recursion_limit = "256"]

mod app;
mod bili;
mod cli;
mod codex;
mod config;
mod delete;
mod gc;
mod magic;
mod model;
mod scan;
mod theme;
mod ui;
mod util;

use std::process::ExitCode;

use clap::Parser;

use cli::Cli;
use config::Config;

fn main() -> ExitCode {
    // 管道输出被截断时安静退出，而不是 panic。
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();

    let result = match cli.command {
        None => Config::load().and_then(app::run),
        Some(_) => cli::run(cli),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("错误：{error:#}");
            ExitCode::FAILURE
        }
    }
}
