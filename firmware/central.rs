#![no_main]
#![no_std]

mod status_adv;

use rmk::macros::rmk_central;

#[rmk_central]
mod keyboard_central {
    use crate::status_adv::StatusAdvProcessor;

    // 注册 Prospector 状态广播处理器。处理器在固件侧把事件收敛成
    // 26 字节载荷并提交给 rmk 的 status-adv feature（见 rmk/src/ble/status_adv.rs）。
    #[register_processor(poll)]
    fn status_adv_processor() -> StatusAdvProcessor {
        StatusAdvProcessor::new()
    }
}
