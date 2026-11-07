//! A short service-type label (mysql, redis, http, ...) derived from the remote
//! port and the service name, ported from v1's `pkg/svctype`, and whether a
//! service speaks HTTP.

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

/// Words of the service name, checked in order: more specific hints
/// (sentinel) come before more general ones (redis).
const NAME_HINTS: &[(&[&str], ServiceType)] = &[
    (&["sentinel"], ServiceType::RedisSentinel),
    (&["redis"], ServiceType::Redis),
    (&["mysql", "mariadb", "maria", "galera"], ServiceType::Mysql),
    (&["postgres", "postgresql", "pgsql"], ServiceType::Postgres),
    (
        &["elastic", "elasticsearch", "kibana"],
        ServiceType::Elasticsearch,
    ),
    (&["mongo", "mongodb"], ServiceType::Mongodb),
    (&["kafka"], ServiceType::Kafka),
    (&["rabbit", "rabbitmq", "amqp"], ServiceType::Rabbitmq),
    (
        &[
            "http", "https", "nginx", "web", "api", "frontend", "gateway", "ingress",
        ],
        ServiceType::Http,
    ),
];

/// Ports on which services with a label of their own speak HTTP: the
/// RabbitMQ management UI, Elasticsearch's REST API and Kibana.
const HTTP_PORTS: &[u16] = &[15672, 9200, 5601];

/// Words of a name that speaks HTTP on any port, whatever its label.
const HTTP_WORDS: &[&str] = &["kibana"];

pub fn classify(service: &str, remote_port: u16) -> ServiceType {
    by_port(remote_port).unwrap_or_else(|| by_name(service))
}

/// Whether the service speaks HTTP, which decides "open in browser" and warm
/// streams: the services labelled http, and the web endpoints of some with a
/// label of their own.
pub fn speaks_http(service: &str, remote_port: u16) -> bool {
    if HTTP_PORTS.contains(&remote_port) {
        return true;
    }
    match by_port(remote_port) {
        Some(typ) => typ == ServiceType::Http,
        None => by_name(service) == ServiceType::Http || has_word(service, HTTP_WORDS),
    }
}

fn by_name(service: &str) -> ServiceType {
    NAME_HINTS
        .iter()
        .find(|(hints, _)| has_word(service, hints))
        .map_or(ServiceType::Other, |(_, typ)| *typ)
}

/// Whether one of the words of a service name, its runs of letters, is one
/// of `hints`, in any case: `orders-api` and `redis6` hold `api` and
/// `redis`, `rapid-cache` holds no `api`.
fn has_word(service: &str, hints: &[&str]) -> bool {
    service
        .split(|c: char| !c.is_ascii_alphabetic())
        .any(|word| hints.iter().any(|hint| hint.eq_ignore_ascii_case(word)))
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
    fn names_match_by_whole_words() {
        for (name, want) in [
            // Hints inside other words count no more.
            ("rapid-cache", ServiceType::Other),
            ("cobweb-db", ServiceType::Other),
            ("capital", ServiceType::Other),
            // Digits and dots separate words, and case does not matter.
            ("redis6", ServiceType::Redis),
            ("orders.api", ServiceType::Http),
            ("Orders-API", ServiceType::Http),
            // The full names of products that v1 found as substrings.
            ("mongodb-0", ServiceType::Mongodb),
            ("rabbitmq", ServiceType::Rabbitmq),
            ("postgresql-ha", ServiceType::Postgres),
            ("mariadb", ServiceType::Mysql),
            ("shop-https", ServiceType::Http),
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

    #[test]
    fn web_endpoints_with_a_label_of_their_own_speak_http() {
        for (name, port, want) in [
            ("shop-web", 8080, true),
            ("orders-api", 12345, true),
            ("rabbitmq", 15672, true),
            ("search", 9200, true),
            ("dashboards", 5601, true),
            ("kibana", 12345, true),
            ("logs-kibana", 80, true),
            // Their other ports do not, nor does a name on a known port.
            ("rabbitmq", 5672, false),
            ("search", 9300, false),
            ("kibana", 3306, false),
            ("web-frontend", 3306, false),
            ("elasticsearch", 12345, false),
            ("rapid-cache", 12345, false),
            ("mystery-service", 55555, false),
        ] {
            assert_eq!(speaks_http(name, port), want, "{name}:{port}");
        }
        // The labels stay as they were.
        assert_eq!(classify("rabbitmq", 15672), ServiceType::Rabbitmq);
        assert_eq!(classify("kibana", 12345), ServiceType::Elasticsearch);
    }
}
