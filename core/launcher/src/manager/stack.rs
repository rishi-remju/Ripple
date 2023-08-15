// Copyright 2023 Comcast Cable Communications Management, LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// SPDX-License-Identifier: Apache-2.0
//

use ripple_sdk::log::debug;

#[derive(Default, Debug, Clone)]
pub struct Stack {
    stack: Vec<String>,
}

impl Stack {
    pub fn new() -> Self {
        Stack { stack: Vec::new() }
    }

    pub fn peek(&self) -> Option<&String> {
        self.stack.last()
    }

    pub fn push(&mut self, item: String) {
        self.stack.push(item);
    }

    pub fn pop_item(&mut self, item: &str) {
        self.stack.retain(|name| name.ne(&item));
    }

    pub fn contains(&mut self, item: &String) -> bool {
        self.stack.contains(item)
    }

    pub fn bring_to_front(&mut self, item: &str) {
        self.pop_item(item);
        self.push(item.to_string());
    }

    pub fn send_to_back(&mut self, item: &str) {
        self.pop_item(item);
        self.stack.insert(0, item.to_string());
    }

    pub fn dump_stack(&mut self) {
        debug!("dump_stack: {:?}", self.stack);
    }

    pub fn len(&self) -> usize {
        self.stack.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stack.len() == 0
    }
}
