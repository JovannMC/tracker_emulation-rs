use firmware_protocol::deku::prelude::*;
use firmware_protocol::{BoardType, ImuType, McuType, Packet, SbPacket};
use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;
use tokio::sync::watch;

pub struct TrackerState {
    pub status: String,
    pub packet_number: u32,
    pub last_received_packet_time: u16,
}

pub enum FirmwareFeatureFlags {
    LegacySlimevrTracker = 0,
    RotationData = 1,
    PositionData = 2,
    None = 9999,
}

pub struct EmulatedTracker {
    // Configuration
    mac_address: [u8; 6],
    firmware_version: String,
    feature_flags: FirmwareFeatureFlags,
    board_type: BoardType,
    imu_type: ImuType,
    mcu_type: McuType,
    server_timeout: u64,
    server_ip: String,
    server_port: u16,

    // Socket stuff
    state: Arc<Mutex<TrackerState>>,
    socket: Option<Arc<UdpSocket>>,
    status_tx: watch::Sender<String>,
    status_rx: watch::Receiver<String>,
}

impl EmulatedTracker {
    pub async fn new(
        mac_address: [u8; 6],
        firmware_version: String,
        feature_flags: Option<FirmwareFeatureFlags>,
        board_type: Option<BoardType>,
        imu_type: Option<ImuType>,
        mcu_type: Option<McuType>,
        server_ip: Option<String>,
        server_discovery_port: Option<u16>,
        server_timeout_ms: Option<u64>,
    ) -> Result<Self, String> {
        // Set default values if the parameters are None
        let feature_flags = feature_flags.unwrap_or(FirmwareFeatureFlags::None);
        let board_type = board_type.unwrap_or(BoardType::Unknown(0));
        let imu_type = imu_type.unwrap_or(ImuType::Unknown(0));
        let mcu_type = mcu_type.unwrap_or(McuType::Unknown(0));
        let server_ip = server_ip.unwrap_or("255.255.255.255".to_string());
        let server_port = server_discovery_port.unwrap_or(6969);
        let server_timeout = server_timeout_ms.unwrap_or(5000);

        let (status_tx, status_rx) = watch::channel("initializing".to_string()); // Create a watch channel

        let state = Arc::new(Mutex::new(TrackerState {
            status: "initializing".to_string(),
            packet_number: 0,
            last_received_packet_time: 0,
        }));

        Ok(Self {
            mac_address,
            firmware_version,
            feature_flags,
            board_type,
            imu_type,
            mcu_type,
            server_timeout,
            server_ip,
            server_port,
            socket: None,
            state,
            status_tx,
            status_rx,
        })
    }

    pub async fn init(&mut self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if state.status != "initializing" {
            return Ok(());
        }
    
        // Create a socket and bind to the local address
        let bind_address = format!("{}:{}", "0.0.0.0", 0); // Bind to all interfaces on a random port
        let socket = UdpSocket::bind(&bind_address)
            .await
            .map_err(|e| format!("Failed to bind socket: {}", e))?;
    
        // Set the broadcast option to true
        socket.set_broadcast(true).map_err(|e| format!("Failed to set broadcast option: {}", e))?;
    
        self.socket = Some(Arc::new(socket));
    
        self.status_tx.send("idle".to_string()).unwrap();
        state.status = "idle".to_string();
        drop(state);
    
        let server_ip = self.server_ip.clone();
        let server_port = self.server_port;
    
        let mut discovery_interval = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut status_rx = self.status_rx.clone();
    
        loop {
            tokio::select! {
                _ = discovery_interval.tick() => {
                    // Send handshake if not connected
                    let state = self.state.lock().unwrap();
                    if state.status != "connected-to-server" {
                        drop(state);
                        self.send_handshake(server_ip.clone(), server_port).await?;
                    } else {
                        break;
                    }
                }
                result = status_rx.changed() => {
                    match result {
                        Ok(_) => {
                            let state = self.state.lock().unwrap();
                            println!("Status changed to: {}", state.status);
                            drop(state);
                        }
                        Err(e) => {
                            println!("Error receiving status update: {}", e);
                            break;
                        }
                    }
                }
            }
        }
    
        Ok(())
    }
    pub async fn deinit(&mut self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if state.status == "initializing" {
            return Ok(());
        }

        self.socket = None;
        self.status_tx.send("initializing".to_string()).unwrap(); // Update status
        state.status = "initializing".to_string();
        drop(state);
        Ok(())
    }

    //fn handle_packet(&self, packet: &[u8]) {}

    fn increment_packet_number(&self) {
        let mut state = self.state.lock().unwrap();
        state.packet_number += 1;
    }

    async fn send_handshake(&self, server_ip: String, server_port: u16) -> Result<(), String> {
        let data = SbPacket::Handshake {
            board: self.clone_board_type(),
            imu: self.clone_imu_type(),
            mcu: self.clone_mcu_type(),
            imu_info: (0, 0, 0),
            build: 1,
            firmware: self.firmware_version.clone().into(),
            mac_address: self.mac_address,
        };
        let packet = Packet::new(0, data);

        let socket = self.socket.as_ref().ok_or("Socket not initialized")?;
        socket
            .send_to(&packet.to_bytes().unwrap(), (server_ip, server_port))
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn clone_board_type(&self) -> BoardType {
        match &self.board_type {
            BoardType::SlimeVRLegacy => BoardType::SlimeVRLegacy,
            BoardType::SlimeVRDev => BoardType::SlimeVRDev,
            BoardType::NodeMCU => BoardType::NodeMCU,
            BoardType::Custom => BoardType::Custom,
            BoardType::WRoom32 => BoardType::WRoom32,
            BoardType::WemosD1Mini => BoardType::WemosD1Mini,
            BoardType::TTGOTBase => BoardType::TTGOTBase,
            BoardType::ESP01 => BoardType::ESP01,
            BoardType::SlimeVR => BoardType::SlimeVR,
            BoardType::LolinC3Mini => BoardType::LolinC3Mini,
            BoardType::Beetle32C3 => BoardType::Beetle32C3,
            BoardType::ESP32C3DevKitM1 => BoardType::ESP32C3DevKitM1,
            BoardType::OwoTrack => BoardType::OwoTrack,
            BoardType::Wrangler => BoardType::Wrangler,
            BoardType::Mocopi => BoardType::Mocopi,
            BoardType::WemosWroom02 => BoardType::WemosWroom02,
            BoardType::XiaoEsp32C3 => BoardType::XiaoEsp32C3,
            BoardType::Haritora => BoardType::Haritora,
            BoardType::ESP32C6DevKitC1 => BoardType::ESP32C6DevKitC1,
            BoardType::GloveImuSlimeVRDev => BoardType::GloveImuSlimeVRDev,
            BoardType::Gestures => BoardType::Gestures,
            BoardType::DevReserved => BoardType::DevReserved,
            BoardType::Unknown(val) => BoardType::Unknown(*val),
            _ => BoardType::Unknown(0),
        }
    }

    fn clone_imu_type(&self) -> ImuType {
        match &self.imu_type {
            ImuType::Mpu9250 => firmware_protocol::ImuType::Mpu9250,
            ImuType::Mpu6500 => firmware_protocol::ImuType::Mpu6500,
            ImuType::Bno080 => firmware_protocol::ImuType::Bno080,
            ImuType::Bno085 => firmware_protocol::ImuType::Bno085,
            ImuType::Bno055 => firmware_protocol::ImuType::Bno055,
            ImuType::Mpu6050 => firmware_protocol::ImuType::Mpu6050,
            ImuType::Bno086 => firmware_protocol::ImuType::Bno086,
            ImuType::Bmi160 => firmware_protocol::ImuType::Bmi160,
            ImuType::Icm20948 => firmware_protocol::ImuType::Icm20948,
            ImuType::Icm42688 => firmware_protocol::ImuType::Icm42688,
            ImuType::Bmi270 => firmware_protocol::ImuType::Bmi270,
            ImuType::Lsm6ds3trc => firmware_protocol::ImuType::Lsm6ds3trc,
            ImuType::Lsm6dsv => firmware_protocol::ImuType::Lsm6dsv,
            ImuType::Lsm6dso => firmware_protocol::ImuType::Lsm6dso,
            ImuType::Lsm6dsr => firmware_protocol::ImuType::Lsm6dsr,
            ImuType::Icm45686 => firmware_protocol::ImuType::Icm45686,
            ImuType::Icm45605 => firmware_protocol::ImuType::Icm45605,
            ImuType::AdcResistance => firmware_protocol::ImuType::AdcResistance,
            ImuType::DevReserved => firmware_protocol::ImuType::DevReserved,
            ImuType::Unknown(val) => firmware_protocol::ImuType::Unknown(*val),
            _ => firmware_protocol::ImuType::Unknown(0),
        }
    }

    fn clone_mcu_type(&self) -> McuType {
        match &self.mcu_type {
            McuType::Esp8266 => McuType::Esp8266,
            McuType::Esp32 => McuType::Esp32,
            McuType::OwoTrackAndroid => McuType::OwoTrackAndroid,
            McuType::Wrangler => McuType::Wrangler,
            McuType::OwoTrackIos => McuType::OwoTrackIos,
            McuType::Esp32_C3 => McuType::Esp32_C3,
            McuType::Mocopi => McuType::Mocopi,
            McuType::Haritora => McuType::Haritora,
            McuType::DevReserved => McuType::DevReserved,
            McuType::Unknown(val) => McuType::Unknown(*val),
            _ => McuType::Unknown(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[tokio::test]
    async fn test_send_handshake() {
        let mac_address = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let firmware_version = "test_firmware".to_string();
        let server_discovery_ip = "255.255.255.255".to_string();
        let server_discovery_port = 6969;

        let mut tracker = EmulatedTracker::new(
            mac_address,
            firmware_version.clone(),
            None,
            None,
            None,
            None,
            Some(server_discovery_ip.clone()),
            Some(server_discovery_port),
            None,
        )
        .await
        .expect("Failed to create EmulatedTracker");

        tracker.init().await.expect("Failed to initialize tracker");

        tracker
            .send_handshake(server_discovery_ip.clone(), server_discovery_port)
            .await
            .expect("Failed to send handshake");

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}