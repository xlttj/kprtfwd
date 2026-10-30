//! A short service-type label (mysql, redis, http, ...) derived from the remote
//! port and the service name. Ported from v1's `pkg/svctype`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceType {
    Mysql,
    Redis,
    RedisSentinel,
    Postgres,
    Elasticsearch,
    Mongodb,
    Kafka,
    Rabbitmq,
    Http,
    Other,
}

impl ServiceType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mysql => "mysql",
            Self::Redis => "redis",
            Self::RedisSentinel => "redis-sentinel",
            Self::Postgres => "postgres",
            Self::Elasticsearch => "elasticsearch",
            Self::Mongodb => "mongodb",
            Self::Kafka => "kafka",
            Self::Rabbitmq => "rabbitmq",
            Self::Http => "http",
            Self::Other => "other",
        }
    }
}

/// Well-known remote ports. The port wins over the name because it is
/// unambiguous for standard deployments.
fn by_port(port: u16) -> Option<ServiceType> {
    Some(match port {
        3306 | 33060 => ServiceType::Mysql,
        6379 => ServiceType::Redis,
        26379 => ServiceType::RedisSentinel,
        5432 => ServiceType::Postgres,
        9200 | 9300 => ServiceType::Elasticsearch,
        27017 => ServiceType::Mongodb,
        9092 => ServiceType::Kafka,
        5672 | 15672 => ServiceType::Rabbitmq,
        80 | 443 | 3000 | 8000 | 8080 | 8443 => ServiceType::Http,
        _ => return None,
    })
}

/// Substrings of the service name, checked in order: more specific hints
/// (sentinel) come before more general ones (redis).
const NAME_HINTS: &[(&[&str], ServiceType)] = &[
    (&["sentinel"], ServiceType::RedisSentinel),
    (&["redis"], ServiceType::Redis),
    (&["mysql", "mariadb", "maria", "galera"], ServiceType::Mysql),
    (&["postgres", "postgre", "pgsql"], ServiceType::Postgres),
    (
        &["elastic", "elasticsearch", "kibana"],
        ServiceType::Elasticsearch,
    ),
    (&["mongo"], ServiceType::Mongodb),
    (&["kafka"], ServiceType::Kafka),
    (&["rabbit", "amqp"], ServiceType::Rabbitmq),
    (
        &[
            "http", "nginx", "web", "api", "frontend", "gateway", "ingress",
        ],
        ServiceType::Http,
    ),
];

pub fn classify(service: &str, remote_port: u16) -> ServiceType {
    if let Some(typ) = by_port(remote_port) {
        return typ;
    }
    let name = service.to_lowercase();
    NAME_HINTS
        .iter()
        .find(|(hints, _)| hints.iter().any(|hint| name.contains(hint)))
        .map_or(ServiceType::Other, |(_, typ)| *typ)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_port() {
        for (name, port, want) in [
            ("anything", 3306, ServiceType::Mysql),
            ("svc", 6379, ServiceType::Redis),
            ("svc", 26379, ServiceType::RedisSentinel),
            ("svc", 5432, ServiceType::Postgres),
            ("svc", 9200, ServiceType::Elasticsearch),
            ("svc", 27017, ServiceType::Mongodb),
            ("svc", 8080, ServiceType::Http),
            ("svc", 443, ServiceType::Http),
        ] {
            assert_eq!(classify(name, port), want, "{name}:{port}");
        }
    }

    #[test]
    fn falls_back_to_name() {
        for (name, want) in [
            ("redis-sentinel-headless", ServiceType::RedisSentinel),
            ("my-redis-master", ServiceType::Redis),
            ("maria-galera", ServiceType::Mysql),
            ("orders-api", ServiceType::Http),
            ("elasticsearch-data", ServiceType::Elasticsearch),
            ("kibana", ServiceType::Elasticsearch),
        ] {
            assert_eq!(classify(name, 12345), want, "{name}");
        }
    }

    #[test]
    fn port_beats_name() {
        assert_eq!(classify("web-frontend", 3306), ServiceType::Mysql);
    }

    #[test]
    fn unknown_is_other() {
        assert_eq!(classify("mystery-service", 55555), ServiceType::Other);
    }
}
