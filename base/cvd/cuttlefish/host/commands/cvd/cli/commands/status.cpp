/*
 * Copyright (C) 2022 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#include "cuttlefish/host/commands/cvd/cli/commands/status.h"

#include <chrono>
#include <iostream>
#include <memory>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <android-base/parseint.h>
#include <android-base/strings.h>
#include <json/value.h>

#include "cuttlefish/common/libs/utils/flag_parser.h"
#include "cuttlefish/common/libs/utils/result.h"
#include "cuttlefish/host/commands/cvd/cli/command_request.h"
#include "cuttlefish/host/commands/cvd/cli/commands/command_handler.h"
#include "cuttlefish/host/commands/cvd/cli/selector/selector.h"
#include "cuttlefish/host/commands/cvd/cli/types.h"
#include "cuttlefish/host/commands/cvd/cli/utils.h"
#include "cuttlefish/host/commands/cvd/instances/instance_manager.h"
#include "cuttlefish/host/commands/cvd/instances/instance_record.h"
#include "cuttlefish/host/libs/config/config_constants.h"

namespace cuttlefish {
namespace {

constexpr char kSummaryHelpText[] =
    "Query status of a single instance group.  Use `cvd fleet` for all devices";

constexpr char kDetailedHelpText[] = R"(

usage: cvd <selector/driver options> <command> <args>

Selector Options:
  -group_name <name>     Specify the name of the instance group created
                         or selected.
  -instance_name <name>  Selects the device of the given name to perform the
                         commands for.
  -instance_name <names> Takes the names of the devices to create within an
                         instance group. The 'names' is comma-separated.

Driver Options:
  -verbosity=<LEVEL>     Adjust Cvd verbosity level. LEVEL is Android log
                         severity. (Required: cvd >= v1.3)

Args:
  --wait_for_launcher    How many seconds to wait for the launcher to respond
                         to the status request. A value of zero means wait
                         indefinitely.
                         (Current value: "5")

  --instance_name        Deprecated, use selectors instead.

  --print                If provided, prints status and instance config
                         information to stdout instead of CHECK.
                         (Current value: "false", Required: Android > 12)

  --help                 List this message

)";

Result<unsigned> IdFromInstanceNameFlag(std::string_view name_or_id) {
  android::base::ConsumePrefix(&name_or_id, kCvdNamePrefix);
  unsigned id;
  CF_EXPECT(android::base::ParseUint(std::string(name_or_id), &id),
            "--instance_name should be either cvd-<id> or id. To use it as a "
            "selector flag it must appear before the subcommand.");
  return id;
}

struct StatusCommandOptions {
  int wait_for_launcher_seconds;
  std::string instance_name;
  bool print;
  bool help;
};

Result<StatusCommandOptions> ParseFlags(cvd_common::Args& args) {
  StatusCommandOptions ret{
      .wait_for_launcher_seconds = 5,
      .instance_name = "",
      .print = false,
      .help = false,
  };
  std::vector<Flag> flags = {
      GflagsCompatFlag("wait_for_launcher", ret.wait_for_launcher_seconds),
      GflagsCompatFlag("instance_name", ret.instance_name),
      GflagsCompatFlag("print", ret.print),
      GflagsCompatFlag("help", ret.help),
  };

  CF_EXPECT(ConsumeFlags(flags, args));

  return ret;
}

}  // namespace

class CvdStatusCommandHandler : public CvdCommandHandler {
 public:
  CvdStatusCommandHandler(InstanceManager& instance_manager);

  Result<void> Handle(const CommandRequest& request) override;
  cvd_common::Args CmdList() const override { return {"status", "cvd_status"}; }

  Result<std::string> SummaryHelp() const override { return kSummaryHelpText; }

  bool ShouldInterceptHelp() const override { return true; }

  Result<std::string> DetailedHelp(std::vector<std::string>&) const override {
    return kDetailedHelpText;
  }

 private:
  InstanceManager& instance_manager_;
};

CvdStatusCommandHandler::CvdStatusCommandHandler(
    InstanceManager& instance_manager)
    : instance_manager_(instance_manager) {}

Result<void> CvdStatusCommandHandler::Handle(const CommandRequest& request) {
  CF_EXPECT(CanHandle(request));

  std::vector<std::string> cmd_args = request.SubcommandArguments();
  StatusCommandOptions flags = CF_EXPECT(ParseFlags(cmd_args));

  if (flags.help) {
    std::cout << kDetailedHelpText << std::endl;
    return {};
  }

  if (!CF_EXPECT(instance_manager_.HasInstanceGroups())) {
    return CF_ERR(NoGroupMessage(request));
  }

  if (request.Selectors().instance_names && !flags.instance_name.empty()) {
    return CF_ERR(
        "The subcommand flag '--instance_name' conflicts with the selector "
        "flag of the same name and can't be used at the same time.");
  }

  Json::Value status_array(Json::arrayValue);

  if (!request.Selectors().instance_names && flags.instance_name.empty()) {
    // No attempt at selecting an instance, get group status instead
    LocalInstanceGroup group =
        CF_EXPECT(selector::SelectGroup(instance_manager_, request));
    status_array = CF_EXPECT(group.FetchStatus(
        std::chrono::seconds(flags.wait_for_launcher_seconds)));
    instance_manager_.UpdateInstanceGroup(group);
  } else {
    std::pair<LocalInstance, LocalInstanceGroup> pair =
        flags.instance_name.empty()
            ? CF_EXPECT(selector::SelectInstance(instance_manager_, request))
            : CF_EXPECT(instance_manager_.FindInstanceWithGroup(
                  {.instance_id = CF_EXPECT(
                       IdFromInstanceNameFlag(flags.instance_name))}));
    LocalInstance instance = pair.first;
    LocalInstanceGroup group = pair.second;
    status_array.append(CF_EXPECT(instance.FetchStatus(
        std::chrono::seconds(flags.wait_for_launcher_seconds))));
    instance_manager_.UpdateInstanceGroup(group);
  }

  if (flags.print) {
    std::cout << status_array.toStyledString();
  }

  return {};
}

std::unique_ptr<CvdCommandHandler> NewCvdStatusCommandHandler(
    InstanceManager& instance_manager) {
  return std::unique_ptr<CvdCommandHandler>(
      new CvdStatusCommandHandler(instance_manager));
}

}  // namespace cuttlefish
