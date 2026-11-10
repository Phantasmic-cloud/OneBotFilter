use onebotfilter::{parse_cli, CliAction, USAGE};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config_path = match parse_cli(&args) {
        Ok(CliAction::Run(path)) => path,
        Ok(CliAction::Version) => {
            println!("onebotfilter {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Ok(CliAction::Help) => {
            print!("{USAGE}");
            return;
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("创建异步运行时失败: {}", e);
            std::process::exit(1);
        }
    };
    if let Err(e) = runtime.block_on(onebotfilter::run(&config_path)) {
        log::error!("{}", e);
        std::process::exit(1);
    }
}
