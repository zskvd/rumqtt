use crate::v5::mqttbytes::PingReq;

use super::mqttbytes::v5::Packet;
use super::mqttbytes::{
    self, PubAck, PubComp, PubRec, PubRel, Publish, QoS, Subscribe, Unsubscribe,
};
use super::{Event, Incoming, Outgoing, Request};

use bytes::BytesMut;
use std::collections::VecDeque;
use std::{io, time::Instant};

/// Errors during state handling
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// Io Error while state is passed to network
    #[error("Io error: {0:?}")]
    Io(#[from] io::Error),
    /// Invalid state for a given operation
    #[error("Invalid state for a given operation")]
    InvalidState,
    /// Received a packet (ack) which isn't asked for
    #[error("Received unsolicited ack pkid: {0}")]
    Unsolicited(u16),
    /// Last pingreq isn't acked
    #[error("Last pingreq isn't acked")]
    AwaitPingResp,
    /// Received a wrong packet while waiting for another packet
    #[error("Received a wrong packet while waiting for another packet")]
    WrongPacket,
    #[error("Timeout while waiting to resolve collision")]
    CollisionTimeout,
    #[error("A Subscribe packet must contain atleast one filter")]
    EmptySubscription,
    #[error("Mqtt serialization/deserialization error: {0}")]
    Deserialization(#[from] mqttbytes::Error),
}

/// State of the mqtt connection.
// Design: Methods will just modify the state of the object without doing any network operations
// Design: All inflight queues are maintained in a pre initialized vec with index as packet id.
// This is done for 2 reasons
// Bad acks or out of order acks aren't O(n) causing cpu spikes
// Any missing acks from the broker are detected during the next recycled use of packet ids
#[derive(Debug, Clone)]
pub struct MqttState {
    /// Status of last ping
    pub await_pingresp: bool,
    /// Collision ping count. Collisions stop user requests
    /// which inturn trigger pings. Multiple pings without
    /// resolving collisions will result in error
    pub collision_ping_count: usize,
    /// Last incoming packet time
    last_incoming: Instant,
    /// Last outgoing packet time
    last_outgoing: Instant,
    /// Packet id of the last outgoing packet
    pub(crate) last_pkid: u16,
    /// Number of outgoing inflight publishes
    pub(crate) inflight: u16,
    /// Maximum number of allowed inflight
    pub(crate) max_inflight: u16,
    /// Outgoing QoS 1, 2 publishes which aren't acked yet
    pub(crate) outgoing_pub: Vec<Option<Publish>>,
    /// Packet ids of released QoS 2 publishes
    pub(crate) outgoing_rel: Vec<Option<u16>>,
    /// Packet ids on incoming QoS 2 publishes
    pub(crate) incoming_pub: Vec<Option<u16>>,
    /// Last collision due to broker not acking in order
    pub collision: Option<Publish>,
    /// Buffered incoming packets
    pub events: VecDeque<Event>,
    /// Write buffer
    pub write: BytesMut,
    /// Indicates if acknowledgements should be send immediately
    pub manual_acks: bool,
}

impl MqttState {
    /// Creates new mqtt state. Same state should be used during a
    /// connection for persistent sessions while new state should
    /// instantiated for clean sessions
    pub fn new(max_inflight: u16, manual_acks: bool) -> Self {
        MqttState {
            await_pingresp: false,
            collision_ping_count: 0,
            last_incoming: Instant::now(),
            last_outgoing: Instant::now(),
            last_pkid: 0,
            inflight: 0,
            max_inflight,
            // index 0 is wasted as 0 is not a valid packet id
            outgoing_pub: vec![None; max_inflight as usize + 1],
            outgoing_rel: vec![None; max_inflight as usize + 1],
            incoming_pub: vec![None; std::u16::MAX as usize + 1],
            collision: None,
            // TODO: Optimize these sizes later
            events: VecDeque::with_capacity(100),
            write: BytesMut::with_capacity(10 * 1024),
            manual_acks,
        }
    }

    /// Returns inflight outgoing packets and clears internal queues
    pub fn clean(&mut self) -> Vec<Request> {
        let mut pending = Vec::with_capacity(100);
        // remove and collect pending publishes
        for publish in self.outgoing_pub.iter_mut() {
            if let Some(publish) = publish.take() {
                let request = Request::Publish(publish);
                pending.push(request);
            }
        }

        // remove and collect pending releases
        for rel in self.outgoing_rel.iter_mut() {
            if let Some(pkid) = rel.take() {
                let request = Request::PubRel(PubRel::new(pkid));
                pending.push(request);
            }
        }

        // remove packed ids of incoming qos2 publishes
        for id in self.incoming_pub.iter_mut() {
            id.take();
        }

        self.await_pingresp = false;
        self.collision_ping_count = 0;
        self.inflight = 0;
        pending
    }

    pub fn inflight(&self) -> u16 {
        self.inflight
    }

    /// Consolidates handling of all outgoing mqtt packet logic. Returns a packet which should
    /// be put on to the network by the eventloop
    pub fn handle_outgoing_packet(&mut self, request: Request) -> Result<(), StateError> {
        match request {
            Request::Publish(publish) => self.outgoing_publish(publish)?,
            Request::PubRel(pubrel) => self.outgoing_pubrel(pubrel)?,
            Request::Subscribe(subscribe) => self.outgoing_subscribe(subscribe)?,
            Request::Unsubscribe(unsubscribe) => self.outgoing_unsubscribe(unsubscribe)?,
            Request::PingReq => self.outgoing_ping()?,
            Request::Disconnect => self.outgoing_disconnect()?,
            Request::PubAck(puback) => self.outgoing_puback(puback)?,
            Request::PubRec(pubrec) => self.outgoing_pubrec(pubrec)?,
            _ => unimplemented!(),
        };

        self.last_outgoing = Instant::now();
        Ok(())
    }

    /// Consolidates handling of all incoming mqtt packets. Returns a `Notification` which for the
    /// user to consume and `Packet` which for the eventloop to put on the network
    /// E.g For incoming QoS1 publish packet, this method returns (Publish, Puback). Publish packet will
    /// be forwarded to user and Pubck packet will be written to network
    pub fn handle_incoming_packet(&mut self, packet: Incoming) -> Result<(), StateError> {
        let out = match &packet {
            Incoming::PingResp(_) => self.handle_incoming_pingresp(),
            Incoming::Publish(publish, _) => self.handle_incoming_publish(publish),
            Incoming::SubAck(..) => self.handle_incoming_suback(),
            Incoming::UnsubAck(_) => self.handle_incoming_unsuback(),
            Incoming::PubAck(puback, _) => self.handle_incoming_puback(puback),
            Incoming::PubRec(pubrec, _) => self.handle_incoming_pubrec(pubrec),
            Incoming::PubRel(pubrel, _) => self.handle_incoming_pubrel(pubrel),
            Incoming::PubComp(pubcomp, _) => self.handle_incoming_pubcomp(pubcomp),
            _ => {
                error!("Invalid incoming packet = {:?}", packet);
                return Err(StateError::WrongPacket);
            }
        };

        out?;
        self.events.push_back(Event::Incoming(packet));
        self.last_incoming = Instant::now();
        Ok(())
    }

    fn handle_incoming_suback(&mut self) -> Result<(), StateError> {
        Ok(())
    }

    fn handle_incoming_unsuback(&mut self) -> Result<(), StateError> {
        Ok(())
    }

    /// Results in a publish notification in all the QoS cases. Replys with an ack
    /// in case of QoS1 and Replys rec in case of QoS while also storing the message
    fn handle_incoming_publish(&mut self, publish: &Publish) -> Result<(), StateError> {
        let qos = publish.qos;

        match qos {
            QoS::AtMostOnce => Ok(()),
            QoS::AtLeastOnce => {
                if !self.manual_acks {
                    let puback = PubAck::new(publish.pkid);
                    self.outgoing_puback(puback)?;
                }
                Ok(())
            }
            QoS::ExactlyOnce => {
                let pkid = publish.pkid;
                self.incoming_pub[pkid as usize] = Some(pkid);

                if !self.manual_acks {
                    let pubrec = PubRec::new(pkid);
                    self.outgoing_pubrec(pubrec)?;
                }
                Ok(())
            }
        }
    }

    fn handle_incoming_puback(&mut self, puback: &PubAck) -> Result<(), StateError> {
        let publish = self
            .outgoing_pub
            .get_mut(puback.pkid as usize)
            .ok_or(StateError::Unsolicited(puback.pkid))?;
        let v = match publish.take() {
            Some(_) => {
                self.inflight -= 1;
                Ok(())
            }
            None => {
                error!("Unsolicited puback packet: {:?}", puback.pkid);
                Err(StateError::Unsolicited(puback.pkid))
            }
        };

        if let Some(publish) = self.check_collision(puback.pkid) {
            self.outgoing_pub[publish.pkid as usize] = Some(publish.clone());
            self.inflight += 1;

            let pkid = publish.pkid;
            Packet::Publish(publish, None).write(&mut self.write)?;
            let event = Event::Outgoing(Outgoing::Publish(pkid));
            self.events.push_back(event);
            self.collision_ping_count = 0;
        }

        v
    }

    fn handle_incoming_pubrec(&mut self, pubrec: &PubRec) -> Result<(), StateError> {
        let publish = self
            .outgoing_pub
            .get_mut(pubrec.pkid as usize)
            .ok_or(StateError::Unsolicited(pubrec.pkid))?;
        match publish.take() {
            Some(_) => {
                // NOTE: Inflight - 1 for qos2 in comp
                self.outgoing_rel[pubrec.pkid as usize] = Some(pubrec.pkid);
                Packet::PubRel(PubRel::new(pubrec.pkid), None).write(&mut self.write)?;

                let event = Event::Outgoing(Outgoing::PubRel(pubrec.pkid));
                self.events.push_back(event);
                Ok(())
            }
            None => {
                error!("Unsolicited pubrec packet: {:?}", pubrec.pkid);
                Err(StateError::Unsolicited(pubrec.pkid))
            }
        }
    }

    fn handle_incoming_pubrel(&mut self, pubrel: &PubRel) -> Result<(), StateError> {
        let publish = self
            .incoming_pub
            .get_mut(pubrel.pkid as usize)
            .ok_or(StateError::Unsolicited(pubrel.pkid))?;
        match publish.take() {
            Some(_) => {
                Packet::PubComp(PubComp::new(pubrel.pkid), None).write(&mut self.write)?;
                let event = Event::Outgoing(Outgoing::PubComp(pubrel.pkid));
                self.events.push_back(event);
                Ok(())
            }
            None => {
                error!("Unsolicited pubrel packet: {:?}", pubrel.pkid);
                Err(StateError::Unsolicited(pubrel.pkid))
            }
        }
    }

    fn handle_incoming_pubcomp(&mut self, pubcomp: &PubComp) -> Result<(), StateError> {
        if let Some(publish) = self.check_collision(pubcomp.pkid) {
            let pkid = publish.pkid;
            Packet::Publish(publish, None).write(&mut self.write)?;
            let event = Event::Outgoing(Outgoing::Publish(pkid));
            self.events.push_back(event);
            self.collision_ping_count = 0;
        }

        let pubrel = self
            .outgoing_rel
            .get_mut(pubcomp.pkid as usize)
            .ok_or(StateError::Unsolicited(pubcomp.pkid))?;
        match pubrel.take() {
            Some(_) => {
                self.inflight -= 1;
                Ok(())
            }
            None => {
                error!("Unsolicited pubcomp packet: {:?}", pubcomp.pkid);
                Err(StateError::Unsolicited(pubcomp.pkid))
            }
        }
    }

    fn handle_incoming_pingresp(&mut self) -> Result<(), StateError> {
        self.await_pingresp = false;
        Ok(())
    }

    /// Adds next packet identifier to QoS 1 and 2 publish packets and returns
    /// it buy wrapping publish in packet
    fn outgoing_publish(&mut self, mut publish: Publish) -> Result<(), StateError> {
        if publish.qos != QoS::AtMostOnce {
            if publish.pkid == 0 {
                publish.pkid = self.next_pkid();
            }

            let pkid = publish.pkid;
            if self
                .outgoing_pub
                .get(publish.pkid as usize)
                .ok_or(StateError::Unsolicited(publish.pkid))?
                .is_some()
            {
                info!("Collision on packet id = {:?}", publish.pkid);
                self.collision = Some(publish);
                let event = Event::Outgoing(Outgoing::AwaitAck(pkid));
                self.events.push_back(event);
                return Ok(());
            }

            // if there is an existing publish at this pkid, this implies that broker hasn't acked this
            // packet yet. This error is possible only when broker isn't acking sequentially
            self.outgoing_pub[pkid as usize] = Some(publish.clone());
            self.inflight += 1;
        };

        debug!(
            "Publish. Topic = {}, Pkid = {:?}, Payload Size = {:?}",
            String::from_utf8(publish.topic.to_vec()).unwrap(),
            publish.pkid,
            publish.payload.len()
        );

        let pkid = publish.pkid;
        Packet::Publish(publish, None).write(&mut self.write)?;
        let event = Event::Outgoing(Outgoing::Publish(pkid));
        self.events.push_back(event);
        Ok(())
    }

    fn outgoing_pubrel(&mut self, pubrel: PubRel) -> Result<(), StateError> {
        let pubrel = self.save_pubrel(pubrel)?;

        debug!("Pubrel. Pkid = {}", pubrel.pkid);
        Packet::PubRel(PubRel::new(pubrel.pkid), None).write(&mut self.write)?;

        let event = Event::Outgoing(Outgoing::PubRel(pubrel.pkid));
        self.events.push_back(event);
        Ok(())
    }

    fn outgoing_puback(&mut self, puback: PubAck) -> Result<(), StateError> {
        let pkid = puback.pkid;
        Packet::PubAck(puback, None).write(&mut self.write)?;
        let event = Event::Outgoing(Outgoing::PubAck(pkid));
        self.events.push_back(event);
        Ok(())
    }

    fn outgoing_pubrec(&mut self, pubrec: PubRec) -> Result<(), StateError> {
        let pkid = pubrec.pkid;
        Packet::PubRec(pubrec, None).write(&mut self.write)?;
        let event = Event::Outgoing(Outgoing::PubRec(pkid));
        self.events.push_back(event);
        Ok(())
    }

    /// check when the last control packet/pingreq packet is received and return
    /// the status which tells if keep alive time has exceeded
    /// NOTE: status will be checked for zero keepalive times also
    fn outgoing_ping(&mut self) -> Result<(), StateError> {
        let elapsed_in = self.last_incoming.elapsed();
        let elapsed_out = self.last_outgoing.elapsed();

        if self.collision.is_some() {
            self.collision_ping_count += 1;
            if self.collision_ping_count >= 2 {
                return Err(StateError::CollisionTimeout);
            }
        }

        // raise error if last ping didn't receive ack
        if self.await_pingresp {
            return Err(StateError::AwaitPingResp);
        }

        self.await_pingresp = true;

        debug!(
            "Pingreq,
            last incoming packet before {} millisecs,
            last outgoing request before {} millisecs",
            elapsed_in.as_millis(),
            elapsed_out.as_millis()
        );

        Packet::PingReq(PingReq).write(&mut self.write)?;
        let event = Event::Outgoing(Outgoing::PingReq);
        self.events.push_back(event);
        Ok(())
    }

    fn outgoing_subscribe(&mut self, mut subscription: Subscribe) -> Result<(), StateError> {
        if subscription.filters.is_empty() {
            return Err(StateError::EmptySubscription);
        }

        let pkid = self.next_pkid();
        subscription.pkid = pkid;

        debug!(
            "Subscribe. Topics = {:?}, Pkid = {:?}",
            subscription.filters, subscription.pkid
        );

        let pkid = subscription.pkid;
        Packet::Subscribe(subscription, None).write(&mut self.write)?;
        let event = Event::Outgoing(Outgoing::Subscribe(pkid));
        self.events.push_back(event);
        Ok(())
    }

    fn outgoing_unsubscribe(&mut self, mut unsub: Unsubscribe) -> Result<(), StateError> {
        let pkid = self.next_pkid();
        unsub.pkid = pkid;

        debug!(
            "Unsubscribe. Topics = {:?}, Pkid = {:?}",
            unsub.filters, unsub.pkid
        );

        let pkid = unsub.pkid;
        Packet::Unsubscribe(unsub).write(&mut self.write)?;
        let event = Event::Outgoing(Outgoing::Unsubscribe(pkid));
        self.events.push_back(event);
        Ok(())
    }

    fn outgoing_disconnect(&mut self) -> Result<(), StateError> {
        debug!("Disconnect");

        Packet::Disconnect(Default::default()).write(&mut self.write)?;
        let event = Event::Outgoing(Outgoing::Disconnect);
        self.events.push_back(event);
        Ok(())
    }

    fn check_collision(&mut self, pkid: u16) -> Option<Publish> {
        if let Some(publish) = &self.collision {
            if publish.pkid == pkid {
                return self.collision.take();
            }
        }

        None
    }

    fn save_pubrel(&mut self, mut pubrel: PubRel) -> Result<PubRel, StateError> {
        let pubrel = match pubrel.pkid {
            // consider PacketIdentifier(0) as uninitialized packets
            0 => {
                pubrel.pkid = self.next_pkid();
                pubrel
            }
            _ => pubrel,
        };

        self.outgoing_rel[pubrel.pkid as usize] = Some(pubrel.pkid);
        self.inflight += 1;
        Ok(pubrel)
    }

    /// http://stackoverflow.com/questions/11115364/mqtt-messageid-practical-implementation
    /// Packet ids are incremented till maximum set inflight messages and reset to 1 after that.
    ///
    fn next_pkid(&mut self) -> u16 {
        let next_pkid = self.last_pkid + 1;

        // When next packet id is at the edge of inflight queue,
        // set await flag. This instructs eventloop to stop
        // processing requests until all the inflight publishes
        // are acked
        if next_pkid == self.max_inflight {
            self.last_pkid = 0;
            return next_pkid;
        }

        self.last_pkid = next_pkid;
        next_pkid
    }
}

#[cfg(test)]
mod test {
    use crate::v5::mqttbytes::v5::Packet;

    use super::mqttbytes::*;
    use super::{Event, Incoming, Outgoing, Request};
    use super::{MqttState, StateError};

    fn build_outgoing_publish(qos: QoS) -> Publish {
        let topic = "hello/world".to_owned();
        let payload = vec![1, 2, 3];

        let mut publish = Publish::new(topic, QoS::AtLeastOnce, payload);
        publish.qos = qos;
        publish
    }

    fn build_incoming_publish(qos: QoS, pkid: u16) -> Publish {
        let topic = "hello/world".to_owned();
        let payload = vec![1, 2, 3];

        let mut publish = Publish::new(topic, QoS::AtLeastOnce, payload);
        publish.pkid = pkid;
        publish.qos = qos;
        publish
    }

    fn build_mqttstate() -> MqttState {
        MqttState::new(100, false)
    }

    #[test]
    fn next_pkid_increments_as_expected() {
        let mut mqtt = build_mqttstate();

        for i in 1..=100 {
            let pkid = mqtt.next_pkid();

            // loops between 0-99. % 100 == 0 implies border
            let expected = i % 100;
            if expected == 0 {
                break;
            }

            assert_eq!(expected, pkid);
        }
    }

    #[test]
    fn outgoing_publish_should_set_pkid_and_add_publish_to_queue() {
        let mut mqtt = build_mqttstate();

        // QoS0 Publish
        let publish = build_outgoing_publish(QoS::AtMostOnce);

        // QoS 0 publish shouldn't be saved in queue
        mqtt.outgoing_publish(publish).unwrap();
        assert_eq!(mqtt.last_pkid, 0);
        assert_eq!(mqtt.inflight, 0);

        // QoS1 Publish
        let publish = build_outgoing_publish(QoS::AtLeastOnce);

        // Packet id should be set and publish should be saved in queue
        mqtt.outgoing_publish(publish.clone()).unwrap();
        assert_eq!(mqtt.last_pkid, 1);
        assert_eq!(mqtt.inflight, 1);

        // Packet id should be incremented and publish should be saved in queue
        mqtt.outgoing_publish(publish).unwrap();
        assert_eq!(mqtt.last_pkid, 2);
        assert_eq!(mqtt.inflight, 2);

        // QoS1 Publish
        let publish = build_outgoing_publish(QoS::ExactlyOnce);

        // Packet id should be set and publish should be saved in queue
        mqtt.outgoing_publish(publish.clone()).unwrap();
        assert_eq!(mqtt.last_pkid, 3);
        assert_eq!(mqtt.inflight, 3);

        // Packet id should be incremented and publish should be saved in queue
        mqtt.outgoing_publish(publish).unwrap();
        assert_eq!(mqtt.last_pkid, 4);
        assert_eq!(mqtt.inflight, 4);
    }

    #[test]
    fn incoming_publish_should_be_added_to_queue_correctly() {
        let mut mqtt = build_mqttstate();

        // QoS0, 1, 2 Publishes
        let publish1 = build_incoming_publish(QoS::AtMostOnce, 1);
        let publish2 = build_incoming_publish(QoS::AtLeastOnce, 2);
        let publish3 = build_incoming_publish(QoS::ExactlyOnce, 3);

        mqtt.handle_incoming_publish(&publish1).unwrap();
        mqtt.handle_incoming_publish(&publish2).unwrap();
        mqtt.handle_incoming_publish(&publish3).unwrap();

        let pkid = mqtt.incoming_pub[3].unwrap();

        // only qos2 publish should be add to queue
        assert_eq!(pkid, 3);
    }

    #[test]
    fn incoming_publish_should_be_acked() {
        let mut mqtt = build_mqttstate();

        // QoS0, 1, 2 Publishes
        let publish1 = build_incoming_publish(QoS::AtMostOnce, 1);
        let publish2 = build_incoming_publish(QoS::AtLeastOnce, 2);
        let publish3 = build_incoming_publish(QoS::ExactlyOnce, 3);

        mqtt.handle_incoming_publish(&publish1).unwrap();
        mqtt.handle_incoming_publish(&publish2).unwrap();
        mqtt.handle_incoming_publish(&publish3).unwrap();

        if let Event::Outgoing(Outgoing::PubAck(pkid)) = mqtt.events[0] {
            assert_eq!(pkid, 2);
        } else {
            panic!("missing puback");
        }

        if let Event::Outgoing(Outgoing::PubRec(pkid)) = mqtt.events[1] {
            assert_eq!(pkid, 3);
        } else {
            panic!("missing PubRec");
        }
    }

    #[test]
    fn incoming_publish_should_not_be_acked_with_manual_acks() {
        let mut mqtt = build_mqttstate();
        mqtt.manual_acks = true;

        // QoS0, 1, 2 Publishes
        let publish1 = build_incoming_publish(QoS::AtMostOnce, 1);
        let publish2 = build_incoming_publish(QoS::AtLeastOnce, 2);
        let publish3 = build_incoming_publish(QoS::ExactlyOnce, 3);

        mqtt.handle_incoming_publish(&publish1).unwrap();
        mqtt.handle_incoming_publish(&publish2).unwrap();
        mqtt.handle_incoming_publish(&publish3).unwrap();

        let pkid = mqtt.incoming_pub[3].unwrap();
        assert_eq!(pkid, 3);

        assert!(mqtt.events.is_empty());
    }

    #[test]
    fn incoming_qos2_publish_should_send_rec_to_network_and_publish_to_user() {
        let mut mqtt = build_mqttstate();
        let publish = build_incoming_publish(QoS::ExactlyOnce, 1);

        mqtt.handle_incoming_publish(&publish).unwrap();
        let packet = Packet::read(&mut mqtt.write, 10 * 1024).unwrap();
        match packet {
            Packet::PubRec(pubrec, _) => assert_eq!(pubrec.pkid, 1),
            _ => panic!("Invalid network request: {:?}", packet),
        }
    }

    #[test]
    fn incoming_puback_should_remove_correct_publish_from_queue() {
        let mut mqtt = build_mqttstate();

        let publish1 = build_outgoing_publish(QoS::AtLeastOnce);
        let publish2 = build_outgoing_publish(QoS::ExactlyOnce);

        mqtt.outgoing_publish(publish1).unwrap();
        mqtt.outgoing_publish(publish2).unwrap();
        assert_eq!(mqtt.inflight, 2);

        mqtt.handle_incoming_puback(&PubAck::new(1)).unwrap();
        assert_eq!(mqtt.inflight, 1);

        mqtt.handle_incoming_puback(&PubAck::new(2)).unwrap();
        assert_eq!(mqtt.inflight, 0);

        assert!(mqtt.outgoing_pub[1].is_none());
        assert!(mqtt.outgoing_pub[2].is_none());
    }

    #[test]
    fn incoming_puback_with_pkid_greater_than_max_inflight_should_be_handled_gracefully() {
        let mut mqtt = build_mqttstate();

        let got = mqtt.handle_incoming_puback(&PubAck::new(101)).unwrap_err();

        match got {
            StateError::Unsolicited(pkid) => assert_eq!(pkid, 101),
            e => panic!("Unexpected error: {}", e),
        }
    }

    #[test]
    fn incoming_pubrec_should_release_publish_from_queue_and_add_relid_to_rel_queue() {
        let mut mqtt = build_mqttstate();

        let publish1 = build_outgoing_publish(QoS::AtLeastOnce);
        let publish2 = build_outgoing_publish(QoS::ExactlyOnce);

        let _publish_out = mqtt.outgoing_publish(publish1);
        let _publish_out = mqtt.outgoing_publish(publish2);

        mqtt.handle_incoming_pubrec(&PubRec::new(2)).unwrap();
        assert_eq!(mqtt.inflight, 2);

        // check if the remaining element's pkid is 1
        let backup = mqtt.outgoing_pub[1].clone();
        assert_eq!(backup.unwrap().pkid, 1);

        // check if the qos2 element's release pkid is 2
        assert_eq!(mqtt.outgoing_rel[2].unwrap(), 2);
    }

    #[test]
    fn incoming_pubrec_should_send_release_to_network_and_nothing_to_user() {
        let mut mqtt = build_mqttstate();

        let publish = build_outgoing_publish(QoS::ExactlyOnce);
        mqtt.outgoing_publish(publish).unwrap();
        let packet = Packet::read(&mut mqtt.write, 10 * 1024).unwrap();
        match packet {
            Packet::Publish(publish, _) => assert_eq!(publish.pkid, 1),
            packet => panic!("Invalid network request: {:?}", packet),
        }

        mqtt.handle_incoming_pubrec(&PubRec::new(1)).unwrap();
        let packet = Packet::read(&mut mqtt.write, 10 * 1024).unwrap();
        match packet {
            Packet::PubRel(pubrel, _) => assert_eq!(pubrel.pkid, 1),
            packet => panic!("Invalid network request: {:?}", packet),
        }
    }

    #[test]
    fn incoming_pubrel_should_send_comp_to_network_and_nothing_to_user() {
        let mut mqtt = build_mqttstate();
        let publish = build_incoming_publish(QoS::ExactlyOnce, 1);

        mqtt.handle_incoming_publish(&publish).unwrap();
        let packet = Packet::read(&mut mqtt.write, 10 * 1024).unwrap();
        match packet {
            Packet::PubRec(pubrec, _) => assert_eq!(pubrec.pkid, 1),
            packet => panic!("Invalid network request: {:?}", packet),
        }

        mqtt.handle_incoming_pubrel(&PubRel::new(1)).unwrap();
        let packet = Packet::read(&mut mqtt.write, 10 * 1024).unwrap();
        match packet {
            Packet::PubComp(pubcomp, _) => assert_eq!(pubcomp.pkid, 1),
            packet => panic!("Invalid network request: {:?}", packet),
        }
    }

    #[test]
    fn incoming_pubcomp_should_release_correct_pkid_from_release_queue() {
        let mut mqtt = build_mqttstate();
        let publish = build_outgoing_publish(QoS::ExactlyOnce);

        mqtt.outgoing_publish(publish).unwrap();
        mqtt.handle_incoming_pubrec(&PubRec::new(1)).unwrap();

        mqtt.handle_incoming_pubcomp(&PubComp::new(1)).unwrap();
        assert_eq!(mqtt.inflight, 0);
    }

    #[test]
    fn outgoing_ping_handle_should_throw_errors_for_no_pingresp() {
        let mut mqtt = build_mqttstate();
        mqtt.outgoing_ping().unwrap();

        // network activity other than pingresp
        let publish = build_outgoing_publish(QoS::AtLeastOnce);
        mqtt.handle_outgoing_packet(Request::Publish(publish))
            .unwrap();
        mqtt.handle_incoming_packet(Incoming::PubAck(PubAck::new(1), None))
            .unwrap();

        // should throw error because we didn't get pingresp for previous ping
        match mqtt.outgoing_ping() {
            Ok(_) => panic!("Should throw pingresp await error"),
            Err(StateError::AwaitPingResp) => (),
            Err(e) => panic!("Should throw pingresp await error. Error = {:?}", e),
        }
    }

    #[test]
    fn outgoing_ping_handle_should_succeed_if_pingresp_is_received() {
        let mut mqtt = build_mqttstate();

        // should ping
        mqtt.outgoing_ping().unwrap();
        mqtt.handle_incoming_packet(Incoming::PingResp(PingResp))
            .unwrap();

        // should ping
        mqtt.outgoing_ping().unwrap();
    }
}
