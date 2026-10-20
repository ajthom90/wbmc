// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub mod auth;
pub mod error;
pub mod events;
pub mod mime;
pub mod model;
pub mod provider;
pub mod secrets;
pub mod setup;
pub mod store;
pub mod sync;

pub use error::{CoreError, Result};
