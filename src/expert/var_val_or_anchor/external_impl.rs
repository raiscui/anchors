/*
 * @Author: Rais
 * @Date: 2023-03-31 15:16:54
 * @LastEditTime: 2023-03-31 15:40:48
 * @LastEditors: Rais
 * @Description:
 */

use crate::singlethread::ValOrAnchor;

#[cfg(feature = "generic_size_voa")]
use emg_common::{GenericSize, LogicLength};

#[cfg(feature = "generic_size_voa")]
impl From<LogicLength> for ValOrAnchor<GenericSize> {
    fn from(value: LogicLength) -> Self {
        Self::Val(value.into())
    }
}
