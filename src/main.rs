fn main() {
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
    if let Err(e) = runtime.block_on(onebotfilter::run("config.yaml")) {
        log::error!("{}", e);
        std::process::exit(1);
    }
}
