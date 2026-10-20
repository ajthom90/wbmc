// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// Schema changes after the v1 baseline, in ascending version order.
pub(super) const STEPS: &[(i64, &str)] = &[(
    2,
    "CREATE INDEX idx_messages_date ON messages (date_ms DESC);",
)];
