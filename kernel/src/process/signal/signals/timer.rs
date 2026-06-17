// SPDX-License-Identifier: MPL-2.0

use super::Signal;
use crate::process::signal::{
    c_types::{sigval_t, siginfo_t},
    constants::SI_TIMER,
    sig_num::SigNum,
};

#[derive(Clone)]
pub struct TimerSignal {
    num: SigNum,
    timer_id: i32,
    overrun: i32,
    value: sigval_t,
}

impl core::fmt::Debug for TimerSignal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TimerSignal")
            .field("num", &self.num)
            .field("timer_id", &self.timer_id)
            .field("overrun", &self.overrun)
            .finish()
    }
}

impl TimerSignal {
    pub fn new(num: SigNum, timer_id: i32, overrun: i32, value: sigval_t) -> Self {
        Self {
            num,
            timer_id,
            overrun,
            value,
        }
    }
}

impl Signal for TimerSignal {
    fn num(&self) -> SigNum {
        self.num
    }

    fn to_info(&self) -> siginfo_t {
        let mut info = siginfo_t::new(self.num, SI_TIMER);
        info.set_timer_fields(self.timer_id, self.overrun);
        info.set_value(self.value);
        info
    }
}
