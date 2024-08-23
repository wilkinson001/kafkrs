use kafkrs_models::config::Config;
use std::error::Error;
use std::fs;
use std::path::Path;

pub(crate) fn load_config(file_path: String) -> Config {
    let res = fs::read_to_string(Path::new(&file_path));

    let contents: String = match res {
        Ok(file) => file,
        Err(error) => panic!("File not found {:?}", error.source()),
    };
    let str_contents: &str = &*contents;
    let config: Config = match toml::from_str(str_contents) {
        Ok(conf) => conf,
        Err(error) => panic!("Config file is not valid: {:?}", error.message()),
    };
    config
}
