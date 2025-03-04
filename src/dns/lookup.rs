use crate::resources::RecordType;
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
        data::{
            Aaaa,
            Cname,
            Mx,
            Ns,
            Txt,
            A,
        },
        Class,
    },
};
use std::{
    net::SocketAddr,
    time::Duration,
};
use tokio::time::sleep;

#[doc(hidden)]
#[allow(dead_code)]
pub async fn wait_for_dns_record(
    domain: &str,
    ip: std::net::Ipv4Addr,
    max_wait: Option<Duration>,
    step: Duration,
    nameserver: SocketAddr,
) -> Result<(), eyre::Error> {
    debug!("Waiting for DNS record to propagate...");
    let start = std::time::Instant::now();

    loop {
        if let Some(max_wait) = max_wait {
            if start.elapsed() > max_wait {
                // it can take a while...
                bail!("DNS record propagation timeout");
            }
        }

        if check_dns_record(domain, ip, nameserver).await? {
            info!("DNS record for {domain:?} propagated successfully");
            break;
        }

        warn!("DNS record not propagated yet. Waiting...");
        sleep(step).await;
    }

    Ok(())
}

#[doc(hidden)]
pub async fn check_dns_record(
    domain: &str,
    ip: std::net::Ipv4Addr,
    nameserver: SocketAddr,
) -> Result<bool, eyre::Error> {
    debug!(?domain, ?ip, "Checking DNS record...");
    match get_a_records(domain, nameserver).await {
        Ok(ips) => Ok(ips.contains(&(A { address: ip }))),
        Err(e) => {
            warn!("Failed to resolve DNS record: {e}");
            sleep(Duration::from_secs(1)).await;
            Ok(false)
        }
    }
}

async fn get_a_records(qname: &str, nameserver: SocketAddr) -> Result<Vec<A>> {
    let config = ClientConfig::with_nameserver(nameserver);
    let mut client = Client::new(config).await?;
    let rrset = client.query_rrset::<A>(qname, Class::IN).await?;
    Ok(rrset.rdata)
}

/// Resolve a DNS record using the specified nameserver. Will stringify the result according to [RFC 1035](https://datatracker.ietf.org/doc/html/rfc1035).
pub async fn resolve(qname: &str, ty: RecordType, nameserver: SocketAddr) -> rsdns::Result<Option<Vec<String>>> {
    debug!(?qname, ?ty, "DNS record lookup...");

    let config = ClientConfig::with_nameserver(nameserver);
    let mut client = Client::new(config).await?;

    let result = match ty {
        RecordType::A => {
            let result = client.query_rrset::<A>(qname, Class::IN).await?;
            result.rdata.iter().map(|a| a.address.to_string()).collect()
        }
        RecordType::AAAA => {
            let result = client.query_rrset::<Aaaa>(qname, Class::IN).await?;
            result.rdata.iter().map(|a| a.address.to_string()).collect()
        }
        RecordType::CNAME => {
            let result = client.query_rrset::<Cname>(qname, Class::IN).await?;
            result.rdata.iter().map(|cname| cname.cname.to_string()).collect()
        }

        RecordType::MX => {
            let result = client.query_rrset::<Mx>(qname, Class::IN).await?;
            result
                .rdata
                .iter()
                .map(|mx| format!("{} {}", mx.preference, mx.exchange))
                .collect()
        }

        RecordType::TXT => {
            let result = client.query_rrset::<Txt>(qname, Class::IN).await?;
            result
                .rdata
                .iter()
                .map(|mx| String::from_utf8_lossy(&mx.text).to_string())
                .collect()
        }

        RecordType::NS => {
            let result = client.query_rrset::<Ns>(qname, Class::IN).await?;
            result.rdata.iter().map(|mx| mx.nsdname.to_string()).collect()
        }

        ty => {
            error!(?ty, "Cannot resolve this record type");
            return Ok(None);
        }
    };

    Ok(Some(result))
}
