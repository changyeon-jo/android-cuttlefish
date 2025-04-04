//
// Copyright (C) 2019 The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#include "cuttlefish/host/commands/run_cvd/launch/grpc_socket_creator.h"

namespace cuttlefish {

GrpcSocketCreator::GrpcSocketCreator(
    const CuttlefishConfig::InstanceSpecific& instance)
    : instance_(instance) {}

std::string GrpcSocketCreator::CreateGrpcSocket(
    const std::string& process_name) {
  auto name_with_ext = process_name + ".sock";
  auto socket_path = instance_.PerInstanceGrpcSocketPath(name_with_ext);

  return socket_path;
}

}  // namespace cuttlefish
