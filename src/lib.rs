use firmware_protocol::deku::prelude::*;
use firmware_protocol::{
    ActionType, BoardType, CbPacket, ImuType, McuType, Packet, SbPacket, SensorDataType,
    SensorStatus, SlimeQuaternion,
};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::net::UdpSocket;
use tokio::sync::watch::{self, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::time::{interval, sleep};

#[derive(Clone)]
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
    //feature_flags: FirmwareFeatureFlags,
    board_type: BoardType,
    mcu_type: McuType,
    server_timeout: u64,
    server_ip: String,
    server_port: u16,
    debug: bool,

    sensors: Vec<Sensor>,

    // Socket stuff
    state: Arc<Mutex<TrackerState>>,
    socket: Option<Arc<UdpSocket>>,
    status_tx: Sender<String>,
    status_rx: Receiver<String>,
}

impl EmulatedTracker {
    pub async fn new(
        mac_address: [u8; 6],
        firmware_version: String,
        //feature_flags: Option<FirmwareFeatureFlags>,
        board_type: Option<BoardType>,
        mcu_type: Option<McuType>,
        server_ip: Option<String>,
        server_discovery_port: Option<u16>,
        server_timeout_ms: Option<u64>,
        debug: Option<bool>,
    ) -> Result<Self, String> {
        // Set default values if the parameters are None
        //let feature_flags = feature_flags.unwrap_or(FirmwareFeatureFlags::None);
        let board_type = board_type.unwrap_or(BoardType::Unknown(0));
        let mcu_type = mcu_type.unwrap_or(McuType::Unknown(0));
        let server_ip = server_ip.unwrap_or("255.255.255.255".to_string());
        let server_port = server_discovery_port.unwrap_or(6969);
        let server_timeout = server_timeout_ms.unwrap_or(5000);
        let debug = debug.unwrap_or(false);

        let (status_tx, status_rx) = watch::channel("initializing".to_string());

        let state = Arc::new(Mutex::new(TrackerState {
            status: "initializing".to_string(),
            packet_number: 0,
            last_received_packet_time: 0,
        }));

        Ok(Self {
            mac_address,
            firmware_version,
            //feature_flags,
            board_type,
            mcu_type,
            sensors: Vec::new(),
            server_timeout,
            server_ip,
            server_port,
            debug,
            socket: None,
            state,
            status_tx,
            status_rx,
        })
    }

    pub async fn get_state(&self) -> TrackerState {
        self.state.lock().await.clone()
    }

    /*
     * Server init functions
     */

    pub async fn init(&mut self) -> Result<(), String> {
        // Only lock to check/update, then drop before await
        {
            let mut state = self.state.lock().await;
            if state.status != "initializing" {
                return Ok(());
            }
            self.status_tx.send("idle".to_string()).unwrap();
            state.status = "idle".to_string();
        }

        let bind_address = format!("{}:{}", "0.0.0.0", 0);
        let socket = UdpSocket::bind(&bind_address)
            .await
            .map_err(|e| format!("Failed to bind socket: {}", e))?;

        socket
            .set_broadcast(true)
            .map_err(|e| format!("Failed to set broadcast option: {}", e))?;

        self.socket = Some(Arc::new(socket));

        let mut discovery_interval = interval(std::time::Duration::from_secs(1));
        let server_timeout = self.server_timeout;

        self.start_heartbeat().await;

        // Track last heartbeat time
        let last_heartbeat = Arc::new(Mutex::new(SystemTime::now()));
        let last_heartbeat_clone = last_heartbeat.clone();
        let server_timeout_clone = server_timeout;
        let state_clone = self.state.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_millis(server_timeout_clone)).await;
                let last = last_heartbeat_clone.lock().await;
                let elapsed = last.elapsed().unwrap_or_default().as_millis() as u64;
                if elapsed > server_timeout_clone {
                    println!("Heartbeat timeout detected (no heartbeat within {server_timeout_clone} ms)");
                    let mut state = state_clone.lock().await;
                    state.status = "initializing".to_string();
                    drop(state);
                }
            }
        });

        loop {
            tokio::select! {
                _ = discovery_interval.tick() => {
                    let state = self.state.lock().await;
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
                                if self.debug {
                                    println!("Received data from: {addr:?}, size: {size}");
                                    println!("Data: {:?}", String::from_utf8_lossy(&buf[..size]));
                                }
                                let mut state = self.state.lock().await;
                                if state.status != "connected-to-server" {
                                    state.status = "connected-to-server".to_string();
                                    self.status_tx.send("connected-to-server".to_string()).unwrap();
                                }
                                state.last_received_packet_time = SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap()
                                    .as_millis() as u16;
                                drop(state);

                                // Update last heartbeat time if a heartbeat is received
                                if let Ok((_rest, packet)) = Packet::from_bytes((&buf[..size], 0)) {
                                    let (_seq, packet_data) = packet.split();
                                    if let CbPacket::Heartbeat = packet_data {
                                        let mut last = last_heartbeat.lock().await;
                                        *last = SystemTime::now();
                                    }
                                }

                                if let Err(e) = self.handle_packet(&buf[..size]).await {
                                    println!("Error handling packet: {e}");
                                }
                            }
                            Err(e) => {
                                println!("Failed to receive data: {e}");
                            }
                        }
                    }
                } => {}
            }
        }

        Ok(())
    }

    pub async fn deinit(&mut self) -> Result<(), String> {
        let mut state = self.state.lock().await;
        if state.status == "initializing" {
            return Ok(());
        }

        self.socket = None;
        self.status_tx.send("initializing".to_string()).unwrap();
        state.status = "initializing".to_string();
        drop(state);
        Ok(())
    }

    async fn handle_packet(&self, data: &[u8]) -> Result<(), String> {
        let (_rest, packet) =
            Packet::from_bytes((data, 0)).map_err(|e| format!("Failed to parse packet: {e}"))?;

        let (_seq, packet_data) = packet.split();

        match packet_data {
            CbPacket::Heartbeat => {
                if self.debug {
                    println!("Received Heartbeat packet");
                }
                let packet_data: SbPacket = SbPacket::Heartbeat {};
                self.send_packet(packet_data).await?
            }
            CbPacket::Ping { challenge } => {
                if self.debug {
                    println!("Received Ping packet with challenge: {:?}", challenge);
                }
                let packet_data: SbPacket = SbPacket::Ping { challenge };
                self.send_packet(packet_data).await?
            }
            CbPacket::Discovery => {
                // println!("Received Discovery packet");
            }
            CbPacket::HandshakeResponse { .. } => {
                //println!("Received HandshakeResponse packet with version: {}", version);
            }
            _ => {
                println!("Received unknown packet: {:?}", packet_data);
            }
        }

        Ok(())
    }

    pub fn subscribe_status(&self) -> Receiver<String> {
        self.status_rx.clone()
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

    // TODO: add these to the firmware_protocol package
    // send_battery_level, send_temperature, send_magnetometer_accuracy, send_signal_strength
    async fn send_sensor_info(&self, sensor: &Sensor) -> Result<(), String> {
        let data = SbPacket::SensorInfo {
            sensor_id: sensor.sensor_id,
            sensor_type: self.clone_sensor_type(&sensor.sensor_type),
            sensor_status: self.clone_sensor_status(&sensor.sensor_status),
        };
        self.send_packet(data).await
    }

    pub async fn send_rotation(
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
        self.send_packet(data).await
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
        self.send_packet(data).await
    }

    pub async fn send_battery_level(&self, percentage: f32, voltage: f32) -> Result<(), String> {
        let data = SbPacket::Battery {
            percentage,
            voltage,
        };
        self.send_packet(data).await
    }

    pub async fn send_temperature(&self, sensor_id: u8, temperature: f32) -> Result<(), String> {
        let data = SbPacket::Temperature {
            sensor_id,
            temperature,
        };
        self.send_packet(data).await
    }

    pub async fn send_signal_strength(&self, sensor_id: u8, strength: i8) -> Result<(), String> {
        let data = SbPacket::SignalStrength {
            sensor_id,
            strength,
        };
        self.send_packet(data).await
    }

    pub async fn send_magnetometer_accuracy(
        &self,
        sensor_id: u8,
        accuracy: f32,
    ) -> Result<(), String> {
        let data = SbPacket::MagAccuracy {
            sensor_id,
            accuracy,
        };
        self.send_packet(data).await
    }

    pub async fn send_user_action(&self, action: ActionType) -> Result<(), String> {
        let data = SbPacket::UserAction { action };
        self.send_packet(data).await
    }

    /*
     * Packet sending functions
     */

    async fn start_heartbeat(&self) {
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
        let debug = self.debug;

        tokio::spawn(async move {
            let result: Result<(), String> = async {
                loop {
                    if status_rx.borrow().as_str() == "initializing" {
                        break;
                    }

                    // gotta manually grab these info instead of using my methods cause self has a limited lifetime
                    // whatever that means man (i kinda get it but not really)
                    let packet_number = {
                        let mut state_lock = state.lock().await;
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

                    if debug {
                        println!("Sending packet: {:?}", packet);
                    }

                    sleep(std::time::Duration::from_secs(1)).await;
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                println!("Error in heartbeat task: {}", e);
            }
        });
    }

    async fn send_packet(&self, data: SbPacket) -> Result<(), String> {
        let packet_number = self.get_packet_number().await?;
        let packet = Packet::new(packet_number, data);

        if self.debug {
            println!("Sending packet: {:?}", packet);
        }

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
        let mut state = self.state.lock().await;
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
            McuType::Esp32C3 => McuType::Esp32C3,
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

    #[tokio::test]
    async fn test_all() {
        use {sleep, Duration};

        let mac_address = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        let firmware_version = "tracker_emulation-rs test".to_string();

        // Create tracker instance
        let mut tracker =
            EmulatedTracker::new(mac_address, firmware_version, None, None, None, None, None)
                .await
                .expect("Failed to create EmulatedTracker");

        tracker.init().await.expect("Failed to initialize tracker");
        sleep(Duration::from_secs(1)).await;

        // Add 5 sensors
        for i in 0..5 {
            tracker
                .add_sensor(ImuType::Mpu6050, SensorStatus::Ok)
                .await
                .expect(&format!("Failed to add sensor {}", i));
            sleep(Duration::from_millis(100)).await;
        }

        sleep(Duration::from_secs(3)).await;

        // Send random rotation and acceleration to each sensor
        for _ in 0..50 {
            for sensor_id in 0..5 {
                let quat = SlimeQuaternion {
                    i: rand::random::<f32>(),
                    j: rand::random::<f32>(),
                    k: rand::random::<f32>(),
                    w: rand::random::<f32>(),
                };
                tracker
                    .send_rotation(sensor_id, SensorDataType::Normal, quat, 42)
                    .await
                    .expect("Failed to send rotation data");

                let accel = (
                    rand::random::<f32>(),
                    rand::random::<f32>(),
                    rand::random::<f32>(),
                );
                tracker
                    .send_acceleration(sensor_id, accel)
                    .await
                    .expect("Failed to send acceleration data");
            }
            sleep(Duration::from_millis(20)).await;
        }

        // Send all user actions
        for action in [
            ActionType::Reset,
            ActionType::ResetMounting,
            ActionType::ResetYaw,
            ActionType::PauseTracking,
        ] {
            let action_type = format!("{:?}", action);
            tracker
                .send_user_action(action)
                .await
                .expect(&format!("Failed to send user action: {:?}", action_type));
            sleep(Duration::from_secs(3)).await;
        }

        // Deinit tracker and reinit
        tracker.deinit().await.expect("Failed to deinit tracker");
        sleep(Duration::from_secs(3)).await;
        tracker
            .init()
            .await
            .expect("Failed to re-initialize tracker");

        // Add another sensor after reinit
        tracker
            .add_sensor(ImuType::Bno080, SensorStatus::Ok)
            .await
            .expect("Failed to add sensor after reinit");

        sleep(Duration::from_secs(3)).await;

        // Send random rotation and acceleration to each sensor
        for _ in 0..50 {
            for sensor_id in 0..5 {
                let quat = SlimeQuaternion {
                    i: rand::random::<f32>(),
                    j: rand::random::<f32>(),
                    k: rand::random::<f32>(),
                    w: rand::random::<f32>(),
                };
                tracker
                    .send_rotation(sensor_id, SensorDataType::Normal, quat, 42)
                    .await
                    .expect("Failed to send rotation data");

                let accel = (
                    rand::random::<f32>(),
                    rand::random::<f32>(),
                    rand::random::<f32>(),
                );
                tracker
                    .send_acceleration(sensor_id, accel)
                    .await
                    .expect("Failed to send acceleration data");
            }
            sleep(Duration::from_millis(20)).await;
        }

        sleep(Duration::from_secs(3)).await;

        tracker
            .deinit()
            .await
            .expect("Failed to deinit tracker at end");
    }
}
