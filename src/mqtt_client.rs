use crate::{
    minimq::{PubInfo, Meta},
    session_state::SessionState,
    ser::serialize,
    de::{
        PacketReader,
        deserialize::{self, ReceivedPacket},
    },
};

use embedded_nal::{Mode, SocketAddr};

use generic_array::GenericArray;
pub use generic_array::ArrayLength;

use nb;

use core::cell::RefCell;
pub use embedded_nal::{IpAddr, Ipv4Addr};

pub struct MqttClient<N, T>
where
    N: embedded_nal::TcpStack,
    T: ArrayLength<u8>,
{
    socket: RefCell<Option<N::TcpSocket>>,
    pub network_stack: N,
    state: SessionState,
    packet_reader: PacketReader<T>,
    transmit_buffer: GenericArray<u8, T>,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum Error<E> {
    Network(E),
    WriteFail,
    Disconnected,
    Invalid,
    Failed,
    Protocol(ProtocolError),
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum ProtocolError {
    Bounds,
    DataSize,
    Invalid,
    PacketSize,
    EmptyPacket,
    Failed,
    PartialPacket,
    InvalidState,
    MalformedPacket,
    MalformedInteger,
    UnknownProperty,
    UnsupportedPacket,
}

impl<E> From<E> for Error<E> {
    fn from(e: E) -> Error<E> {
        Error::Network(e)
    }
}

impl<N, T> MqttClient<N, T>
where
    N: embedded_nal::TcpStack,
    T: ArrayLength<u8>,
{
    pub fn new<'a>(
        broker: IpAddr,
        client_id: &'a str,
        network_stack: N,
    ) -> Result<Self, Error<N::Error>> {
        // Connect to the broker's TCP port.
        let socket = network_stack.open(Mode::NonBlocking)?;

        // Next, connect to the broker over MQTT.
        let socket = network_stack.connect(socket, SocketAddr::new(broker, 1883))?;

        let mut client = MqttClient {
            network_stack: network_stack,
            socket: RefCell::new(Some(socket)),
            state: SessionState::new(broker, client_id),
            transmit_buffer: GenericArray::default(),
            packet_reader: PacketReader::new(),
        };

        client.reset()?;

        Ok(client)
    }

    fn read(&self, mut buf: &mut [u8]) -> Result<usize, Error<N::Error>> {
        let mut socket_ref = self.socket.borrow_mut();
        let mut socket = socket_ref.take().unwrap();
        let read = nb::block!(self.network_stack.read(&mut socket, &mut buf)).unwrap();

        // Put the socket back into the option.
        socket_ref.replace(socket);

        Ok(read)
    }

    fn write(&self, buf: &[u8]) -> Result<(), Error<N::Error>> {
        let mut socket_ref = self.socket.borrow_mut();
        let mut socket = socket_ref.take().unwrap();
        let written = nb::block!(self.network_stack.write(&mut socket, &buf)).unwrap();

        // Put the socket back into the option.
        socket_ref.replace(socket);

        if written != buf.len() {
            Err(Error::WriteFail)
        } else {
            Ok(())
        }
    }

    // TODO: Add subscribe support.

    pub fn publish<'b>(&mut self, topic: &'b str, data: &[u8]) -> Result<(), Error<N::Error>> {
        // If the socket is not connected, we can't do anything.
        if self.socket_is_connected()? == false {
            return Ok(());
        }

        let mut pub_info = PubInfo::new();
        pub_info.topic = Meta::new(topic.as_bytes());

        let len = serialize::publish_message(&mut self.transmit_buffer, &pub_info, data).map_err(|e| Error::Protocol(e))?;
        self.write(&self.transmit_buffer[..len])
    }

    fn socket_is_connected(&self) -> Result<bool, N::Error> {
        let mut socket_ref = self.socket.borrow_mut();
        let socket = socket_ref.take().unwrap();

        let connected = self.network_stack.is_connected(&socket)?;

        socket_ref.replace(socket);

        Ok(connected)
    }

    fn reset(&mut self) -> Result<(), Error<N::Error>> {
        // TODO: Handle connection failures?
        self.connect_socket(true)?;
        self.state.reset();
        self.packet_reader.reset();

        let len = serialize::connect_message(&mut self.transmit_buffer, self.state.client_id.as_str().as_bytes(), self.state.keep_alive_interval).map_err(|e| Error::Protocol(e))?;
        self.write(&self.transmit_buffer[..len])?;

        Ok(())
    }

    fn connect_socket(&mut self, new_socket: bool) -> Result<(), Error<N::Error>> {
        let mut socket_ref = self.socket.borrow_mut();
        let socket = socket_ref.take().unwrap();

        // Close the socket. We need to reset the socket state.
        let socket = if new_socket {
            self.network_stack.close(socket)?;
            self.network_stack.open(Mode::NonBlocking)?
        } else {
            socket
        };

        // Connect to the broker's TCP port with a new socket.
        // TODO: Limit the time between connect attempts to prevent network spam.
        let socket = self
            .network_stack
            .connect(socket, SocketAddr::new(self.state.broker, 1883))?;

        // Store the new socket for future use.
        socket_ref.replace(socket);

        Ok(())
    }

    fn handle_packet<F>(&mut self, packet: ReceivedPacket, f: &F) -> Result<(), Error<N::Error>>
    where
        F: Fn(&PubInfo, &[u8]),
    {
        if !self.state.connected {
            if let ReceivedPacket::ConnAck(acknowledge) = packet {
                if acknowledge.reason_code != 0 {
                    return Err(Error::Failed);
                }

                if acknowledge.session_present {
                    // We do not currently support saved session state.
                    return Err(Error::Failed);
                } else {
                    self.state.reset();
                }

                self.state.connected = true;

                // TODO: Handle properties in the ConnAck.

                return Ok(());
            } else {
                // It is a protocol error to receive anything else when not connected.
                // TODO: Verify it is a protocol error.
                return Err(Error::Protocol(ProtocolError::Invalid));
            }
        }

        match packet {
            ReceivedPacket::Publish(info) => {

                // Call a handler function to deal with the received data.
                let payload = self.packet_reader.payload().map_err(|e| Error::Protocol(e))?;
                f(&info, payload);

                Ok(())
            },

            ReceivedPacket::SubAck(subscribe_acknowledge) => {
                if subscribe_acknowledge.packet_identifier != self.state.get_packet_identifier() {
                    return Err(Error::Invalid);
                }

                if subscribe_acknowledge.reason_code != 0 {
                    return Err(Error::Failed);
                }

                self.state.increment_packet_identifier();

                Ok(())
            },

            _ => Err(Error::Invalid),
        }
    }

    pub fn poll<F>(&mut self, f: F) -> Result<(), Error<N::Error>>
    where
        F: Fn(&PubInfo, &[u8]),
    {
        // If the socket is not connected, we can't do anything.
        if self.socket_is_connected()? == false {
            // TODO: Handle a session timeout.
            self.reset()?;

            return Ok(());
        }

        let mut buf: [u8; 1024] = [0; 1024];
        let received = self.read(&mut buf)?;
        let mut processed = 0;
        while processed < received {

            match self.packet_reader.slurp(&buf[processed..received]) {
                Ok(count) => processed += count,

                Err(_) => {
                    // TODO: We should handle recoverable errors better.
                    self.reset()?;
                    return Err(Error::Disconnected);
                },
            }

            // Handle any received packets.
            if self.packet_reader.packet_available() {

                // TODO: Handle deserialize errors properly.
                let packet = deserialize::parse_message(&mut self.packet_reader).map_err(|e| Error::Protocol(e))?;
                self.packet_reader.pop_packet().map_err(|e| Error::Protocol(e))?;
                self.handle_packet(packet, &f)?;
            }
        }

        Ok(())
    }
}
