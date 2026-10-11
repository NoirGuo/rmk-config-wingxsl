//! Wingxsl 的 Prospector 兼容状态广播处理器。
//!
//! 职责：把 RMK 事件收敛为 `rmk_status_adv::StatusSnapshot`，编码成 26 字节
//! Prospector 载荷，提交给 RMK 的 `status-adv` feature 广播。协议与调度常量
//! 见 `rmk-status-adv` crate；本文件只做「事件 → 快照」的映射与活动跟踪。
//!
//! 与 `zmk-config-Noirix44@monitor`（Prospector 接收器）逐字节兼容，接收器
//! 零改动可用。

use rmk::ble::status_adv::{mark_activity, request_burst, set_payload};
use rmk::event::{
    BatteryStatusEvent, ChargingStateEvent, ConnectionStatusChangeEvent, KeyboardEvent,
    LayerChangeEvent, ModifierEvent, PeripheralBatteryEvent, SleepStateEvent, WpmUpdateEvent,
};
use rmk::macros::processor;
use rmk::types::battery::{BatteryStatus, ChargeState};
use rmk::types::connection::UsbState;
use rmk_status_adv::protocol::{
    encode_layer_name, keyboard_id_from_deviceid, ROLE_CENTRAL,
};
use rmk_status_adv::snapshot::StatusSnapshot;

/// 层名表（与 keyboard.toml `[[keymap.layer]]` 的 name/顺序一致）。
/// 载荷的 layer_name 字段最多 3 个可见字符 + NUL。
const LAYER_NAMES: [&[u8; 4]; 5] = [
    b"BAS\0", // base  → "BAS"
    b"FN1\0",
    b"FN2\0",
    b"FN3\0",
    b"FN4\0",
];

/// 从 nRF52840 FICR 读取 8 字节 DEVICEID（只读，恒定）。
///
/// FICR 基址 0x1000_0000，DEVICEID 位于 0x1000_0060（低 32 位）与
/// 0x1000_0064（高 32 位）。这是稳定的芯片常量，按数据手册地址读取；
/// 折叠成 4 字节键盘指纹后写入载荷（同一芯片所有固件一致）。
fn read_device_id() -> [u8; 8] {
    // SAFETY: FICR 是只读恒定 ROM 区，地址与宽度由 nRF52840 数据手册定义；
    // 读取无副作用、无并发问题。
    let lo = unsafe { core::ptr::read_volatile(0x1000_0060 as *const u32) };
    let hi = unsafe { core::ptr::read_volatile(0x1000_0064 as *const u32) };
    let mut devid = [0u8; 8];
    devid[0..4].copy_from_slice(&lo.to_le_bytes());
    devid[4..8].copy_from_slice(&hi.to_le_bytes());
    devid
}

#[processor(
    subscribe = [
        KeyboardEvent,
        LayerChangeEvent,
        WpmUpdateEvent,
        ModifierEvent,
        BatteryStatusEvent,
        ChargingStateEvent,
        ConnectionStatusChangeEvent,
        PeripheralBatteryEvent,
        SleepStateEvent
    ],
    poll_interval = 20
)]
pub struct StatusAdvProcessor {
    snap: StatusSnapshot,
    /// 会话内是否连接过 BLE（粘性，用于 BLE_BONDED 标志的近似）。
    ever_ble_connected: bool,
    initialized: bool,
}

impl StatusAdvProcessor {
    pub fn new() -> Self {
        let mut snap = StatusSnapshot::default();
        snap.device_role = ROLE_CENTRAL; // Wingxsl 中央侧广播
        snap.device_index = 0;
        snap.keyboard_id = keyboard_id_from_deviceid(&read_device_id());
        snap.layer_name = LAYER_NAMES[0];
        snap.channel = 0; // 0 = 广播给所有接收器
        Self {
            snap,
            ever_ble_connected: false,
            initialized: false,
        }
    }

    /// 提交当前快照（可选突发）。
    fn commit(&self, burst: bool) {
        set_payload(&self.snap.encode(), burst);
    }
}

impl StatusAdvProcessor {
    async fn poll(&mut self) {
        // 首次启动即提交一次，确保接收器能立即看到键盘。
        if !self.initialized {
            self.initialized = true;
            self.commit(false);
            request_burst(); // 开机突发，让接收器尽快锁定
        }
        // 活动时间只由 set_payload / mark_activity 在 rmk 侧刷新；
        // 空闲降级（active 1s → idle 30s）由 rmk 任务按
        // ACTIVITY_TIMEOUT_MS 自动处理，poll 无需干预。
    }

    async fn on_keyboard_event(&mut self, _event: KeyboardEvent) {
        // 按键是活动但不改变载荷字段：只把节奏切回 active。
        mark_activity();
    }

    async fn on_layer_change_event(&mut self, event: LayerChangeEvent) {
        let layer = (event.0 & 0x0F) as usize;
        self.snap.active_layer = event.0;
        self.snap.layer_name = if layer < LAYER_NAMES.len() {
            *LAYER_NAMES[layer]
        } else {
            encode_layer_name(b"L")
        };
        self.commit(true); // 层切换 → 突发
    }

    async fn on_wpm_update_event(&mut self, event: WpmUpdateEvent) {
        self.snap.wpm_value = event.0.min(255) as u8;
        self.commit(false);
    }

    async fn on_modifier_event(&mut self, event: ModifierEvent) {
        self.snap.modifier_flags = event.modifier.into_bits();
        // 修饰键变化高频，不触发突发（与 Prospector 注释一致）。
        self.commit(false);
    }

    async fn on_battery_status_event(&mut self, event: BatteryStatusEvent) {
        match event.0 {
            BatteryStatus::Available {
                charge_state,
                level,
            } => {
                self.snap.battery_level = level.unwrap_or(0).min(100);
                if charge_state == ChargeState::Charging {
                    self.snap.charging = true;
                }
            }
            BatteryStatus::Unavailable => {
                self.snap.battery_level = 0;
            }
        }
        self.commit(false);
    }

    async fn on_charging_state_event(&mut self, event: ChargingStateEvent) {
        self.snap.charging = event.charging;
        self.commit(false);
    }

    async fn on_connection_status_change_event(&mut self, event: ConnectionStatusChangeEvent) {
        let status = event.0;
        // USB 状态 → usb_connected / usb_hid_ready
        self.snap.usb_connected = matches!(
            status.usb,
            UsbState::Configured | UsbState::Suspended | UsbState::Enabled
        );
        self.snap.usb_hid_ready = matches!(status.usb, UsbState::Configured | UsbState::Suspended);
        // BLE 状态 → profile / connected / bonded
        if status.ble.state == rmk::types::ble::BleState::Connected {
            self.snap.ble_connected = true;
            self.ever_ble_connected = true;
        } else {
            self.snap.ble_connected = false;
        }
        self.snap.ble_bonded = self.ever_ble_connected;
        self.snap.profile = status.ble.profile;
        // connection_count：USB 就绪 + BLE 连接（0-5）
        self.snap.connection_count =
            u8::from(self.snap.usb_hid_ready) + u8::from(self.snap.ble_connected);
        self.commit(true); // 连接状态变化 → 突发
    }

    async fn on_peripheral_battery_event(&mut self, event: PeripheralBatteryEvent) {
        if event.id < 3 {
            match event.state.0 {
                BatteryStatus::Available {
                    charge_state,
                    level,
                } => {
                    self.snap.peripheral_battery[event.id] = level.unwrap_or(0).min(100);
                    if event.id == 0 && charge_state == ChargeState::Charging {
                        // 外设充电也算充电状态（可选；Prospector 的 charging
                        // 标志指本机，这里保持本机语义，不覆盖）。
                    }
                }
                BatteryStatus::Unavailable => {
                    self.snap.peripheral_battery[event.id] = 0;
                }
            }
            self.commit(false);
        }
    }

    async fn on_sleep_state_event(&mut self, event: SleepStateEvent) {
        if event.0 {
            // 睡眠：停止广播更省电；载荷保持有效，唤醒后自动恢复。
            set_payload(None, false);
        } else {
            self.commit(false);
        }
    }
}
