// SPDX-License-Identifier: BUSL-1.1

//! Unit tests of `crate::config`, one file per concern.

#[cfg(test)]
mod effective_backend_tests;
#[cfg(test)]
mod env_ref_roundtrip_tests;
#[cfg(test)]
mod env_ref_typing_tests;
#[cfg(test)]
mod env_shadow_config_tests;
#[cfg(test)]
mod general;
#[cfg(test)]
mod log_level_env_tests;
#[cfg(test)]
mod prod_shape_tests;
#[cfg(test)]
mod review2_tests;
