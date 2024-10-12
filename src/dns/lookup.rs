use eyre::{
    bail,
    Result,
};
use rsdns::{
    clients::{
        tokio::Client,
        ClientConfig,
    },
    records::{
        data::A,
        Class,
    },
};
use std::{
    collections::HashSet,
    net::SocketAddr,
    time::Duration,
};
use tokio::time::sleep;

#[allow(dead_code)]
pub async fn wait_for_dns_record(
    domain: &str,
    ip: std::net::Ipv4Addr,
    max_wait: Option<Duration>,
) -> Result<(), eyre::Error> {
    debug!("Waiting for DNS record to propagate...");
    let start = std::time::Instant::now();
    let expected = A { address: ip };

    loop {
        if let Some(max_wait) = max_wait {
            if start.elapsed() > max_wait {
                // it can take a while...
                bail!("DNS record propagation timeout");
            }
        }

        let ips = match get_a_records(domain).await {
            Ok(ips) => ips.into_iter().collect::<HashSet<_>>(),
            Err(e) => {
                warn!("Failed to resolve DNS record: {e}");
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        if !ips.contains(&expected) {
            warn!("DNS record not propagated yet. Waiting...");
            sleep(Duration::from_secs(3)).await;
            continue;
        }

        info!("DNS record for {domain:?} propagated successfully");
        break;
    }

    Ok(())
}

const CLOUDFLARE_NAMESERVER_IP: &str = "1.1.1.1:53";

async fn get_a_records(qname: &str) -> Result<Vec<A>> {
    // use Google's Public DNS recursor as nameserver
    let nameserver: SocketAddr = CLOUDFLARE_NAMESERVER_IP.parse()?;

    // default client configuration; specify nameserver address only
    let config = ClientConfig::with_nameserver(nameserver);

    // create tokio Client
    let mut client = Client::new(config).await?;

    // issue an A query
    let rrset = client.query_rrset::<A>(qname, Class::IN).await?;

    Ok(rrset.rdata)
}
