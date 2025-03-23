use clap::Parser;
use cloudflare_dns_operator::{
    dns::cloudflare::{
        self,
        cloudflare_api_request,
        CloudflareApi,
        DnsRecordInfo,
    },
    resources::RecordType,
};
use eyre::{
    bail,
    Result,
};

#[derive(Parser)]
pub enum Command {
    ListZones(ListZonesArgs),
    ListDnsRecords(ListDnsRecordsArgs),
    UpdateDnsRecord(UpdateRecordArgs),
    CreateDnsRecord(CreateRecordArgs),
    DeleteDnsRecord(DeleteRecordArgs),
}

#[derive(Parser)]
pub struct ListZonesArgs {
    #[clap(short, long, env = "CLOUDFLARE_API_TOKEN")]
    pub api_token: String,
}

#[derive(Parser)]
pub struct ListDnsRecordsArgs {
    #[clap(short, long, env = "CLOUDFLARE_API_TOKEN")]
    pub api_token: String,

    #[clap(env = "CLOUDFLARE_ZONE_ID")]
    pub zone_identifier: String,
}

#[derive(Parser)]
pub struct UpdateRecordArgs {
    #[clap(short, long, env = "CLOUDFLARE_API_TOKEN")]
    pub api_token: String,

    #[clap(short, long, env = "CLOUDFLARE_ZONE_ID")]
    pub zone_identifier: String,

    #[clap(short, long)]
    pub record_identifier: String,

    #[clap(short, long)]
    pub ttl: Option<i64>,

    #[clap()]
    pub content: String,
}

#[derive(Parser)]
pub struct CreateRecordArgs {
    #[clap(short, long, env = "CLOUDFLARE_API_TOKEN")]
    pub api_token: String,

    #[clap(env = "CLOUDFLARE_ZONE_ID")]
    pub zone_name: String,

    #[clap(long)]
    pub name: String,

    #[clap(long)]
    pub record_type: RecordType,

    #[clap(long)]
    pub content: String,

    #[clap(long)]
    pub ttl: Option<i64>,
}

#[derive(Parser)]
pub struct DeleteRecordArgs {
    #[clap(short, long, env = "CLOUDFLARE_API_TOKEN")]
    pub api_token: String,

    #[clap(env = "CLOUDFLARE_ZONE_ID")]
    pub zone_identifier: String,

    #[clap(long = "id")]
    pub record_identifier: Option<String>,

    #[clap(long)]
    pub name: Option<String>,
}

#[tokio::main]
async fn main() {
    color_eyre::install().expect("color_eyre init");
    tracing_subscriber::fmt::init();

    run(Command::parse()).await.unwrap();
}

pub async fn run(cmd: Command) -> Result<()> {
    match cmd {
        Command::ListZones(ListZonesArgs { api_token }) => {
            let url = "https://api.cloudflare.com/client/v4/zones";
            let records =
                cloudflare_api_request::<Vec<serde_json::Value>, ()>(url, None, reqwest::Method::GET, api_token)
                    .await?;
            println!("{}", serde_json::to_string_pretty(&records)?);
        }

        Command::ListDnsRecords(ListDnsRecordsArgs {
            api_token,
            zone_identifier,
        }) => {
            let cloudflare_api = CloudflareApi::new(api_token);
            let records = cloudflare_api.list_dns_records(zone_identifier).await?;
            for record in records {
                let DnsRecordInfo {
                    id,
                    name,
                    record_type,
                    content,
                    ..
                } = record;
                println!("name={name} type={record_type} content={content} id={id}");
            }
        }

        Command::UpdateDnsRecord(_) => todo!(),

        Command::CreateDnsRecord(args) => {
            let cloudflare_api = CloudflareApi::new(args.api_token);
            let result = cloudflare_api
                .create_dns_record(cloudflare::CreateRecordArgs {
                    zone: cloudflare::Zone::Name(args.zone_name),
                    name: args.name,
                    record_type: args.record_type,
                    content: args.content,
                    comment: None,
                    ttl: args.ttl,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }

        Command::DeleteDnsRecord(DeleteRecordArgs {
            api_token,
            zone_identifier,
            record_identifier,
            name,
        }) => {
            let cloudflare_api = CloudflareApi::new(api_token);
            match (record_identifier, name) {
                (None, None) => bail!("must specify either record_identifier or name"),
                (Some(record_identifier), _) => {
                    cloudflare_api
                        .delete_dns_record(zone_identifier, record_identifier)
                        .await?;
                }
                (None, Some(name)) => {
                    cloudflare_api
                        .delete_dns_record_by_name(&name, &zone_identifier)
                        .await?;
                }
            };
        }
    }

    Ok(())
}
