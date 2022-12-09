use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use image_rs::config::ImageConfig;
use image_rs::image::ImageClient;
use image_rs::snapshots;
use kata_sys_util::validate;
use protocols::image;
use tokio::sync::Mutex;
use ttrpc::{self, error::get_rpc_status as ttrpc_error};

const CONTAINER_BASE: &str = "/run/enclave-cc/containers";

pub struct ImageService {
    image_client: Arc<Mutex<ImageClient>>,
}

impl ImageService {
    pub fn new() -> Self {
        let new_config = ImageConfig {
            default_snapshot: snapshots::SnapshotType::OcclumUnionfs,
            ..Default::default()
        };
        Self {
            image_client: Arc::new(Mutex::new(ImageClient {
                config: new_config,
                ..Default::default()
            })),
        }
    }

    async fn pull_image(&self, req: &image::PullImageRequest) -> Result<String> {
        let image = req.get_image();
        let cid = self.get_container_id(req)?;

        let keyprovider_config = Path::new("/etc").join("ocicrypt_keyprovider_native.conf");
        if !keyprovider_config.exists() {
            let config = r#"
            {
                "key-providers": {
                    "attestation-agent": {
                        "native": "attestation-agent"
                    }
                }
            }
            "#;
            File::create(&keyprovider_config)?.write_all(config.as_bytes())?;
        }
        std::env::set_var("OCICRYPT_KEYPROVIDER_CONFIG", keyprovider_config);

        let args: Vec<String> = env::args().collect();
        // If config file specified in the args, read contents from config file
        let config_position = args.iter().position(|a| a == "--decrypt-config" || a == "-c");
        let config = if let Some(config_position) = config_position {
            if let Some(config_file) = args.get(config_position + 1) {
                let cfg = File::open(config_file)?;
                let cfg_parsed: serde_json::Value = serde_json::from_reader(cfg)?;
                Some(cfg_parsed)
            } else {
                panic!("The config argument wasn't formed properly: {:?}", args)
            }
        } else {
            None
        };

        let decrypt_config = if let Some(cfg) = config.clone() {
            if let Some(v) = cfg["security_validate"].as_bool() {
                std::env::set_var("ENABLE_SECURITY_VALIDATE", &format!("{}", v));
            } else {
                panic!("Expect bool true or false ");
            }

            cfg["key_provider"].clone()
        } else {
            serde_json::Value::Null
        };

        let dec_cfg = if !decrypt_config.is_null() {
            decrypt_config.as_str()
        } else {
            None
        };

        let source_creds = (!req.get_source_creds().is_empty()).then(|| req.get_source_creds());

        let bundle_path = Path::new(CONTAINER_BASE).join(&cid);

        println!("Pulling {:?}", image);
        self.image_client
            .lock()
            .await
            .pull_image(image, &bundle_path, &source_creds, &dec_cfg)
            .await?;

        Ok(image.to_owned())
    }

    fn get_container_id(&self, req: &image::PullImageRequest) -> Result<String> {
        let cid = req.get_container_id().to_string() ;
        // keep consistent with the kata container convention, more details
        // are described in https://github.com/confidential-containers/enclave-cc/issues/15
        validate::verify_id(&cid)?;
        Ok(cid)
    }
}

#[async_trait]
impl protocols::image_ttrpc::Image for ImageService {
    async fn pull_image(
        &self,
        _ctx: &ttrpc::r#async::TtrpcContext,
        req: image::PullImageRequest,
    ) -> ttrpc::Result<image::PullImageResponse> {
        match self.pull_image(&req).await {
            Ok(r) => {
                println!("Pull image {:?} successfully", r.clone());
                let mut resp = image::PullImageResponse::new();
                resp.image_ref = r;
                return Ok(resp);
            }
            Err(e) => {
                return Err(ttrpc_error(ttrpc::Code::INTERNAL, e.to_string()));
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_get_container_id() {
        struct ParseCase{
            req: image::PullImageRequest,
            is_ok: bool,
        }
        let cases: Vec<ParseCase> = vec![
            ParseCase{ 
                req: image::PullImageRequest{ 
                    container_id: "redis".to_string(), 
                    ..Default::default()
                }, 
                is_ok: true, 
            },
            ParseCase{ 
                req: image::PullImageRequest{ 
                    container_id: "redis_1.3".to_string(), 
                    ..Default::default()
                }, 
                is_ok: true, 
            },
            ParseCase{ 
                req: image::PullImageRequest{ 
                    container_id: "redis:1.3".to_string(), 
                    ..Default::default()
                }, 
                is_ok: false, 
            },
            ParseCase{ 
                req: image::PullImageRequest{ 
                    container_id: "".to_string(), 
                    ..Default::default()
                }, 
                is_ok: false, 
            },
        ];

        let is = ImageService::new();
        for c in cases {
            assert_eq!(is.get_container_id(&c.req).is_ok(), c.is_ok);
        }
    }
}