use simple_dns::{
    rdata::RData, Name, PacketBuf, PacketHeader, Question, ResourceRecord, QCLASS, QTYPE,
};

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

use crate::{
    conversion_utils::{hashmap_to_txt, ip_addr_to_resource_record, port_to_srv_record},
    dns_packet_receiver::DnsPacketReceiver,
    resource_record_manager::ResourceRecordManager,
    sender_socket,
    simple_responder::build_reply,
    SimpleMdnsError,
};

/// Service Discovery implementation using DNS-SD.
/// This implementation advertise all the registered addresses, query for the same service on the same network and
/// keeps a cache of known service instances
///
/// Notice that this crate does not provide any means of finding your own ip address. There are crates that provide this kind of feature.
///
/// ## Example
/// ```
/// use simple_mdns::ServiceDiscovery;
/// use std::net::SocketAddr;
/// use std::str::FromStr;
///
/// let mut discovery = ServiceDiscovery::new("a", "_mysrv._tcp.local", 60).expect("Invalid Service Name");
/// discovery.add_service_info(SocketAddr::from_str("192.168.1.22:8090").unwrap().into());
///
/// ```
pub struct ServiceDiscovery {
    full_name: Name<'static>,
    service_name: Name<'static>,
    resource_manager: Arc<RwLock<ResourceRecordManager<'static>>>,
    resource_ttl: u32,
    packets_sender: Sender<(PacketBuf, SocketAddr)>,
}

impl ServiceDiscovery {
    /// Creates a new ServiceDiscovery by providing `instance`, `service_name`, `resource ttl` and loopback activation.
    /// `instance_name` and `service_name` will be composed together in order to advertise this instance, like `instance_name`.`service_name`
    ///
    /// `instance_name` must be in the standard specified by the mdns RFC and short, example: **_my_inst**
    /// `service_name` must be in the standard specified by the mdns RFC, example: **_my_service._tcp.local**
    /// `resource_ttl` refers to the amount of time in seconds your service will be cached in the dns responder.
    /// set `enable_loopback` to true if you may have more than one instance of your service running in the same machine
    pub fn new(
        instance_name: &'static str,
        service_name: &'static str,
        resource_ttl: u32,
    ) -> Result<Self, SimpleMdnsError> {
        let full_name = format!("{}.{}", instance_name, service_name);
        let full_name = Name::new(&full_name)?.into_owned();
        let service_name = Name::new(service_name)?;

        let mut resource_manager = ResourceRecordManager::new();
        resource_manager.add_owned_resource(ResourceRecord::new(
            service_name.clone(),
            simple_dns::CLASS::IN,
            0,
            RData::PTR(service_name.clone()),
        ));

        let (tx, rx) = channel();
        let service_discovery = Self {
            full_name,
            service_name,
            resource_manager: Arc::new(RwLock::new(resource_manager)),
            resource_ttl,
            packets_sender: tx.clone(),
        };

        send_packages_loop(rx);

        service_discovery.receive_packets_loop(tx.clone())?;
        service_discovery.refresh_known_instances(tx.clone());

        query_service_instances(service_discovery.service_name.clone(), &tx);

        Ok(service_discovery)
    }

    /// Add the  service info to discovery and immediately advertise the service
    pub fn add_service_info(&mut self, service_info: InstanceInformation) {
        {
            let mut resource_manager = self.resource_manager.write().unwrap();
            for resource in service_info
                .into_records(&self.full_name.clone(), self.resource_ttl)
                .unwrap()
            {
                resource_manager.add_owned_resource(resource);
            }
        }

        self.advertise_service(&self.packets_sender);
    }

    /// Remove all addresses from service discovery
    pub fn remove_service_from_discovery(&mut self) {
        self.resource_manager.write().unwrap().clear();
    }

    /// Return the addresses of all known services
    pub fn get_known_services(&self) -> Vec<InstanceInformation> {
        self.resource_manager
            .read()
            .unwrap()
            .get_domain_resources(&self.service_name, true, false)
            .map(|domain_resources| {
                let mut ip_addresses: Vec<IpAddr> = Vec::new();
                let mut ports = Vec::new();
                let mut attributes = HashMap::new();

                for resource in domain_resources {
                    match &resource.rdata {
                        simple_dns::rdata::RData::A(a) => {
                            ip_addresses.push(Ipv4Addr::from(a.address).into())
                        }
                        simple_dns::rdata::RData::AAAA(aaaa) => {
                            ip_addresses.push(Ipv6Addr::from(aaaa.address).into())
                        }
                        simple_dns::rdata::RData::TXT(txt) => attributes.extend(txt.attributes()),
                        simple_dns::rdata::RData::SRV(srv) => ports.push(srv.port),
                        _ => {}
                    }
                }

                InstanceInformation {
                    ip_addresses,
                    ports,
                    attributes,
                }
            })
            .collect()
    }

    fn refresh_known_instances(&self, packet_sender: Sender<(PacketBuf, SocketAddr)>) {
        let service_name = self.service_name.clone();
        let resource_manager = self.resource_manager.clone();

        std::thread::spawn(move || {
            let service_name = service_name;
            loop {
                let now = Instant::now();
                log::info!("Refreshing known services");

                let next_expiration = resource_manager.read().unwrap().get_next_expiration();

                log::debug!("next expiration: {:?}", next_expiration);
                match next_expiration {
                    Some(expiration) => {
                        if expiration <= now {
                            query_service_instances(service_name.clone(), &packet_sender);
                            std::thread::sleep(Duration::from_secs(5));
                        } else {
                            std::thread::sleep(expiration - now);
                        }
                    }
                    None => {
                        std::thread::sleep(Duration::from_secs(5));
                    }
                }
            }
        });
    }

    fn advertise_service(&self, packet_sender: &Sender<(PacketBuf, SocketAddr)>) {
        log::info!("Advertising service");
        let mut packet = PacketBuf::new(PacketHeader::new_query(0, false), false);
        let _ = packet.add_question(&Question::new(
            self.full_name.clone(),
            QTYPE::SRV,
            QCLASS::IN,
            true,
        ));

        if build_reply(
            packet,
            *super::MULTICAST_IPV4_SOCKET,
            &self.resource_manager.read().unwrap(),
        )
        .and_then(|response| packet_sender.send(response).ok())
        .is_none()
        {
            log::info!("Failed to advertise service");
        }
    }

    fn receive_packets_loop(
        &self,
        packet_sender: Sender<(PacketBuf, SocketAddr)>,
    ) -> Result<(), SimpleMdnsError> {
        let service_name = self.service_name.clone();
        let full_name = self.full_name.clone();
        let resources = self.resource_manager.clone();

        let mut receiver = DnsPacketReceiver::new()?;

        std::thread::spawn(move || loop {
            match receiver.recv_packet() {
                Ok((header, packet, addr)) => {
                    if header.query {
                        match build_reply(packet, addr, &resources.read().unwrap()) {
                            Some(reply_packet) => {
                                log::debug!("sending reply");
                                if packet_sender.send(reply_packet).is_err() {
                                    log::error!("Failed to send reply");
                                }
                            }
                            None => {
                                log::debug!("No reply to send");
                            }
                        }
                    } else {
                        add_response_to_resources(
                            packet,
                            &service_name,
                            &full_name,
                            &mut resources.write().unwrap(),
                        )
                    }
                }
                Err(_) => {
                    log::error!("Received Invalid Packet");
                }
            }
        });

        Ok(())
    }
}

fn query_service_instances(service_name: Name, packet_sender: &Sender<(PacketBuf, SocketAddr)>) {
    log::info!("probing service instances");
    let mut packet = PacketBuf::new(PacketHeader::new_query(0, false), true);
    packet
        .add_question(&Question::new(service_name, QTYPE::SRV, QCLASS::IN, false))
        .unwrap();

    if let Err(err) = packet_sender.send((packet, *super::MULTICAST_IPV4_SOCKET)) {
        log::error!("There was an error sending the question packet: {}", err);
    }
}

fn send_packages_loop(receiver: Receiver<(PacketBuf, SocketAddr)>) {
    let socket = sender_socket(&super::MULTICAST_IPV4_SOCKET).unwrap();
    std::thread::spawn(move || {
        while let Ok((packet, address)) = receiver.recv() {
            if let Err(err) = socket.send_to(&packet, address) {
                log::error!("There was an error sending the question packet: {}", err);
            }
        }
    });
}

fn add_response_to_resources(
    packet: PacketBuf,
    service_name: &Name<'_>,
    full_name: &Name<'_>,
    owned_resources: &mut ResourceRecordManager,
) {
    let packet = match packet.to_packet() {
        Ok(packet) => packet,
        Err(err) => {
            log::error!("Received Invalid packet: {}", err);
            log::debug!("{:?}", packet);
            return;
        }
    };

    let resources = packet
        .answers
        .into_iter()
        .chain(packet.additional_records.into_iter())
        .filter(|aw| {
            aw.name.ne(full_name)
                && aw.name.is_subdomain_of(service_name)
                && (aw.match_qtype(QTYPE::SRV)
                    || aw.match_qtype(QTYPE::TXT)
                    || aw.match_qtype(QTYPE::A)
                    || aw.match_qtype(QTYPE::PTR))
        });

    for resource in resources {
        owned_resources.add_expirable_resource(resource.into_owned());
    }
}

/// Represents a single instance of the service.
/// Notice that it is not possible to associate a port to a single ip address, due to limitations of the DNS protocol
#[derive(Debug)]
pub struct InstanceInformation {
    /// Ips for this instance
    pub ip_addresses: Vec<IpAddr>,
    /// Ports for this instance
    pub ports: Vec<u16>,
    /// Attributes for this instance
    pub attributes: HashMap<String, Option<String>>,
}

impl Default for InstanceInformation {
    fn default() -> Self {
        Self::new()
    }
}

impl InstanceInformation {
    /// Creates an empty InstanceInformation
    pub fn new() -> Self {
        Self {
            ip_addresses: Vec::new(),
            ports: Vec::new(),
            attributes: HashMap::new(),
        }
    }

    /// Transform into a [Vec<ResourceRecord>](`Vec<ResourceRecord>`)
    pub fn into_records<'a>(
        self,
        service_name: &Name<'a>,
        ttl: u32,
    ) -> Result<Vec<ResourceRecord<'a>>, crate::SimpleMdnsError> {
        let mut records = Vec::new();

        for ip_address in self.ip_addresses {
            records.push(ip_addr_to_resource_record(service_name, ip_address, ttl));
        }

        for port in self.ports {
            records.push(port_to_srv_record(service_name, port, ttl));
        }

        records.push(hashmap_to_txt(service_name, self.attributes, ttl)?);

        Ok(records)
    }

    /// Creates a Iterator of [`SocketAddr`](`std::net::SocketAddr`) for each ip address and port combination
    pub fn get_socket_addresses(&'_ self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.ip_addresses
            .iter()
            .copied()
            .map(move |addr| {
                self.ports
                    .iter()
                    .copied()
                    .map(move |port| SocketAddr::new(addr, port))
            })
            .flatten()
    }
}

impl std::hash::Hash for InstanceInformation {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ip_addresses.hash(state);
        self.ports.hash(state);
    }
}

impl From<SocketAddr> for InstanceInformation {
    fn from(addr: SocketAddr) -> Self {
        let ip_address = addr.ip();
        let port = addr.port();

        Self {
            ip_addresses: vec![ip_address],
            ports: vec![port],
            attributes: HashMap::new(),
        }
    }
}
