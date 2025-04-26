use firmware_protocol::deku::prelude::*;
use firmware_protocol::{
    BoardType, ImuType, McuType, Packet, SbPacket, SensorDataType, SensorStatus, SlimeQuaternion,
};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::net::UdpSocket;
use tokio::sync::watch;

pub struct TrackerState {
    pub status: String,
    pub packet_number: u64,
    pub last_received_packet_time: u16,
}

pub enum FirmwareFeatureFlags {
    LegacySlimevrTracker = 0,
    RotationData = 1,
    PositionData = 2,
    None = 9999,
}

#[derive(Debug)]
pub struct Sensor {
    pub sensor_id: u8,
    pub sensor_type: ImuType,
    pub sensor_status: SensorStatus,
}

pub struct EmulatedTracker {
    // Configuration
    mac_address: [u8; 6],
    firmware_version: String,
    feature_flags: FirmwareFeatureFlags,
    board_type: BoardType,
    mcu_type: McuType,
    server_timeout: u64,
    server_ip: String,
    server_port: u16,

    sensors: Vec<Sensor>,

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
        mcu_type: Option<McuType>,
        server_ip: Option<String>,
        server_discovery_port: Option<u16>,
        server_timeout_ms: Option<u64>,
    ) -> Result<Self, String> {
        // Set default values if the parameters are None
        let feature_flags = feature_flags.unwrap_or(FirmwareFeatureFlags::None);
        let board_type = board_type.unwrap_or(BoardType::Unknown(0));
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
            mcu_type,
            sensors: Vec::new(),
            server_timeout,
            server_ip,
            server_port,
            socket: None,
            state,
            status_tx,
            status_rx,
        })
    }

    /*
     * Server init functions
     */

    pub async fn init(&mut self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if state.status != "initializing" {
            return Ok(());
        }

        let bind_address = format!("{}:{}", "0.0.0.0", 0);
        let socket = UdpSocket::bind(&bind_address)
            .await
            .map_err(|e| format!("Failed to bind socket: {}", e))?;

        socket
            .set_broadcast(true)
            .map_err(|e| format!("Failed to set broadcast option: {}", e))?;

        self.socket = Some(Arc::new(socket));

        self.status_tx.send("idle".to_string()).unwrap();
        state.status = "idle".to_string();
        drop(state);

        let mut discovery_interval = tokio::time::interval(std::time::Duration::from_secs(1));

        self.start_heartbeat().await;

        loop {
            tokio::select! {
                _ = discovery_interval.tick() => {
                    let state = self.state.lock().unwrap();
                    if state.status != "connected-to-server" {
                        drop(state);
                        self.send_handshake().await?;
                    } else {
                        break;
                    }
                }

                _ = async {
                    if let Some(socket) = self.socket.as_ref() {
                        let mut buf = [0u8; 1024];
                        match socket.recv_from(&mut buf).await {
                            Ok((size, addr)) => {
                                println!("Received data from: {:?}, size: {}", addr, size);
                                println!("Data: {:?}", String::from_utf8_lossy(&buf[..size]));
                                let mut state = self.state.lock().unwrap();
                                if state.status != "connected-to-server" {
                                    state.status = "connected-to-server".to_string();
                                    self.status_tx.send("connected-to-server".to_string()).unwrap();
                                }
                                state.last_received_packet_time = SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap()
                                    .as_millis() as u16;
                                drop(state);

                                // function to handle data
                                //
                            }
                            Err(e) => {
                                println!("Failed to receive data: {}", e);
                            }
                        }
                    }
                } => {}
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
        self.status_tx.send("initializing".to_string()).unwrap();
        state.status = "initializing".to_string();
        drop(state);
        Ok(())
    }

    /*
     * Tracker functions
     */

    pub async fn add_sensor(
        &mut self,
        sensor_type: ImuType,
        sensor_status: SensorStatus,
    ) -> Result<(), String> {
        let sensor_id = self.sensors.len() as u8;
        let sensor = Sensor {
            sensor_id,
            sensor_type,
            sensor_status,
        };
        self.send_sensor_info(&sensor).await?;
        self.sensors.push(sensor);
        Ok(())
    }

    /*
     * Packet sending functions
     */

    pub async fn start_heartbeat(&self) {
        let socket = match self.socket.as_ref() {
            Some(s) => s.clone(),
            None => {
                println!("Socket not initialized, cannot start heartbeat");
                return;
            }
        };
        let status_rx = self.status_rx.clone();
        let server_ip = self.server_ip.clone();
        let server_port = self.server_port;
        let state = self.state.clone();

        tokio::spawn(async move {
            let result: Result<(), String> = async {
                loop {
                    if status_rx.borrow().as_str() == "initializing" {
                        break;
                    }

                    // gotta manually grab these info instead of using my methods cause self has a limited lifetime
                    // whatever that means man (i kinda get it but not really)
                    let packet_number = {
                        let mut state_lock = state.lock().unwrap();
                        state_lock.packet_number += 1;
                        state_lock.packet_number
                    };
                    let packet = Packet::new(packet_number, SbPacket::Heartbeat);

                    // send heartbeat
                    if let Err(e) = socket
                        .send_to(
                            &packet.to_bytes().unwrap(),
                            (server_ip.as_str(), server_port),
                        )
                        .await
                    {
                        println!("Failed to send heartbeat packet: {e}");
                    }

                    println!("Heartbeat packet sent: {:?}", packet);

                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                println!("Error in heartbeat task: {}", e);
            }
        });
    }

    // send_battery_level, send_rotation_data, send_acceleration, send_temperature, send_magnetometer_accuracy, send_signal_strength, send_user_action
    pub async fn send_sensor_info(&self, sensor: &Sensor) -> Result<(), String> {
        let data = SbPacket::SensorInfo {
            sensor_id: sensor.sensor_id,
            sensor_type: self.clone_sensor_type(&sensor.sensor_type),
            sensor_status: self.clone_sensor_status(&sensor.sensor_status),
        };

        let packet_number = self.get_packet_number().await?;
        let packet = Packet::new(packet_number, data);

        self.send_packet(packet).await
    }

    pub async fn send_rotation_data(
        &self,
        sensor_id: u8,
        data_type: SensorDataType,
        rotation_data: SlimeQuaternion,
        accuracy: u8,
    ) -> Result<(), String> {
        let data = SbPacket::RotationData {
            sensor_id,
            data_type,
            quat: rotation_data,
            calibration_info: accuracy,
        };

        let packet_number = self.get_packet_number().await?;
        let packet = Packet::new(packet_number, data);

        self.send_packet(packet).await
    }

    pub async fn send_acceleration(
        &self,
        sensor_id: u8,
        acceleration: (f32, f32, f32),
    ) -> Result<(), String> {
        let data = SbPacket::Acceleration {
            sensor_id,
            vector: acceleration,
        };

        let packet_number = self.get_packet_number().await?;
        let packet = Packet::new(packet_number, data);

        self.send_packet(packet).await
    }

    /*
     * Packet sending utilities
     */

    async fn send_packet(&self, packet: Packet<SbPacket>) -> Result<(), String> {
        println!("Sending packet: {:?}", packet);

        let socket = self.socket.as_ref().expect("Socket not initialized");
        socket
            .send_to(
                &packet.to_bytes().unwrap(),
                (self.server_ip.clone(), self.server_port),
            )
            .await
            .map_err(|e| e.to_string())?;

        Ok(())
    }

    async fn send_handshake(&self) -> Result<(), String> {
        let data = SbPacket::Handshake {
            board: self.clone_board_type(),
            imu: self.clone_sensor_type(&ImuType::Unknown(0)),
            mcu: self.clone_mcu_type(),
            imu_info: (0, 0, 0),
            build: 13, // current version is 13 apparently
            firmware: self.firmware_version.clone().into(),
            mac_address: self.mac_address,
        };
        let packet = Packet::new(0, data);

        let socket = self.socket.as_ref().ok_or("Socket not initialized")?;
        socket
            .send_to(
                &packet.to_bytes().unwrap(),
                (self.server_ip.clone(), self.server_port),
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn get_packet_number(&self) -> Result<u64, String> {
        let mut state = self.state.lock().unwrap();
        state.packet_number += 1;
        Ok(state.packet_number)
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

    fn clone_sensor_type(&self, imu_type: &ImuType) -> ImuType {
        match imu_type {
            ImuType::Mpu9250 => ImuType::Mpu9250,
            ImuType::Mpu6500 => ImuType::Mpu6500,
            ImuType::Bno080 => ImuType::Bno080,
            ImuType::Bno085 => ImuType::Bno085,
            ImuType::Bno055 => ImuType::Bno055,
            ImuType::Mpu6050 => ImuType::Mpu6050,
            ImuType::Bno086 => ImuType::Bno086,
            ImuType::Bmi160 => ImuType::Bmi160,
            ImuType::Icm20948 => ImuType::Icm20948,
            ImuType::Icm42688 => ImuType::Icm42688,
            ImuType::Bmi270 => ImuType::Bmi270,
            ImuType::Lsm6ds3trc => ImuType::Lsm6ds3trc,
            ImuType::Lsm6dsv => ImuType::Lsm6dsv,
            ImuType::Lsm6dso => ImuType::Lsm6dso,
            ImuType::Lsm6dsr => ImuType::Lsm6dsr,
            ImuType::Icm45686 => ImuType::Icm45686,
            ImuType::Icm45605 => ImuType::Icm45605,
            ImuType::AdcResistance => ImuType::AdcResistance,
            ImuType::DevReserved => ImuType::DevReserved,
            ImuType::Unknown(val) => ImuType::Unknown(*val),
            _ => ImuType::Unknown(0),
        }
    }

    fn clone_sensor_status(&self, status: &SensorStatus) -> SensorStatus {
        match status {
            SensorStatus::Ok => SensorStatus::Ok,
            SensorStatus::Offline => SensorStatus::Offline,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[tokio::test]
    async fn test_send_sensor_data() {
        // tracker setup
        let mac_address = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let firmware_version = "test_firmware".to_string();

        let mut tracker = EmulatedTracker::new(
            mac_address,
            firmware_version,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("Failed to create EmulatedTracker");

        tracker.init().await.expect("Failed to initialize tracker");

        // add sensor
        tracker
            .add_sensor(ImuType::Mpu9250, SensorStatus::Ok)
            .await
            .expect("Failed to add sensor");

        // random sensor data (rotation/accel)
        for _ in 0..150 {
            let quat = SlimeQuaternion {
                i: rand::random::<f32>() * 2.0 - 1.0,
                j: rand::random::<f32>() * 2.0 - 1.0,
                k: rand::random::<f32>() * 2.0 - 1.0,
                w: rand::random::<f32>() * 2.0 - 1.0,
            };

            tracker
                .send_rotation_data(
                    0,
                    SensorDataType::Normal,
                    quat,
                    100, // idk the range of accuracy it should be
                )
                .await
                .expect("Failed to send rotation data");

            let accel = (
                rand::random::<f32>() * 20.0 - 10.0,
                rand::random::<f32>() * 20.0 - 10.0,
                rand::random::<f32>() * 20.0 - 10.0,
            );

            tracker
                .send_acceleration(0, accel)
                .await
                .expect("Failed to send acceleration data");

            tokio::time::sleep(Duration::from_millis(40)).await;
        }

        tokio::time::sleep(Duration::from_secs(3)).await; // wait a few secs to test heartbeats lol
    }
}
