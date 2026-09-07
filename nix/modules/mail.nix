# Public host settings and runtime-only secret references for the concrete SES
# adapter. Rust remains the authority for mailbox, URL and provider validation.
{
  lib,
  pkgs,
  settings,
  runtimeDir,
  pathType,
}:
let
  inherit (lib) mkOption types;
  enabled = settings.mode == "ses";
  boundedText =
    maximum:
    types.addCheck types.str (
      value:
      value != ""
      && builtins.stringLength value <= maximum
      && lib.trim value == value
      && builtins.match ".*[[:cntrl:]].*" value == null
    );
  optional =
    type: description:
    mkOption {
      type = types.nullOr type;
      default = null;
      inherit description;
    };
  publicField =
    maximum: description:
    mkOption {
      type = boundedText maximum;
      inherit description;
    };
  regionType = types.addCheck (boundedText 32) (
    value: builtins.match "[a-z]{2}-[a-z]+-[0-9]+" value != null && !(lib.hasPrefix "cn-" value)
  );
  resourceType = types.addCheck types.str (value: builtins.match "[A-Za-z0-9_-]{1,64}" value != null);
  protectedFiles = lib.optionalAttrs enabled {
    # These names cannot collide with the SSH projections, which always end
    # in -key or -hosts. No secret content is evaluated into a derivation.
    mail-ses = settings.credentialFile;
    mail-controls = settings.controlSigningKeyFile;
  };
  subscriptions = settings.subscriptions;
  feedback = settings.feedback;
in
{
  option = mkOption {
    default = { };
    description = "Public SES settings and protected runtime credential references. Keep subscription admission paused until deployment acceptance.";
    type = types.submodule {
      options = {
        mode = mkOption {
          type = types.enum [
            "disabled"
            "ses"
          ];
          default = "disabled";
          description = "Mail adapter. Disabled loads no mail credentials; use SES with paused subscriptions to preserve existing removal links.";
        };
        sender = optional (boundedText 254) "Verified public ASCII sender mailbox without a display name.";
        region = optional regionType "Commercial SES region.";
        configurationSet = optional resourceType "SES configuration set that publishes tagged delivery feedback.";
        credentialFile = optional pathType "Protected runtime JSON file containing the SES access key, secret key, and optional session token.";
        controlSigningKeyFile = optional pathType "Protected runtime file containing the stable 64-character lowercase hexadecimal subscriber-control key.";
        maxCampaignRecipients = mkOption {
          type = types.ints.between 1 100000;
          default = 2000;
          description = "Maximum recipients admitted for one approved campaign.";
        };
        maxDailyMessages = mkOption {
          type = types.ints.between 1 1000000;
          default = 5000;
          description = "Persisted daily admission budget across newsletters and confirmations.";
        };
        maxDailyConfirmationMessages = mkOption {
          type = types.ints.between 1 1000000;
          default = 100;
          description = "Confirmation admission budget within the total daily budget.";
        };
        sendIntervalMilliseconds = mkOption {
          type = types.ints.between 100 60000;
          default = 1000;
          description = "Minimum interval between serial message admission opportunities.";
        };
        subscriptions =
          optional
            (types.submodule {
              options = {
                mode = mkOption {
                  type = types.enum [
                    "paused"
                    "enabled"
                  ];
                  default = "paused";
                  description = "Pause new admission while keeping confirmation and removal controls available.";
                };
                operatorName = publicField 200 "Public mailing-list operator name.";
                postalAddress = publicField 500 "Public physical postal address included in every message.";
                purpose = publicField 2000 "Public explanation of the mailing list and consent purpose.";
                privacyUrl = mkOption {
                  type = types.addCheck (boundedText 2048) (value: lib.hasPrefix "https://" value);
                  description = "Public HTTPS privacy notice URL without embedded credentials.";
                };
                contactAddress = publicField 254 "Monitored public ASCII contact mailbox.";
              };
            })
            "Public consent disclosures; null provides campaign review without subscriber capture or removal routes.";
        feedback = optional (types.submodule {
          options = {
            queueUrl = publicField 256 "Canonical regional HTTPS URL for the dedicated standard SQS queue.";
            topicArn = publicField 512 "Dedicated standard SNS topic ARN in the same region and account as the queue.";
          };
        }) "Authenticated SES feedback source. Required for enabled subscriptions.";
      };
    };
  };

  assertions = [
    {
      assertion =
        !enabled
        || lib.all (value: value != null) [
          settings.sender
          settings.region
          settings.configurationSet
          settings.credentialFile
          settings.controlSigningKeyFile
        ];
      message = "Maincopy SES requires sender, region, configurationSet, credentialFile, and controlSigningKeyFile.";
    }
    {
      assertion = !enabled || settings.maxDailyConfirmationMessages <= settings.maxDailyMessages;
      message = "Maincopy confirmation messages must fit within the total daily mail budget.";
    }
    {
      assertion =
        !enabled || subscriptions == null || subscriptions.mode != "enabled" || feedback != null;
      message = "Maincopy enabled subscriptions require authenticated feedback configuration.";
    }
  ];

  hostConfig = {
    mode = settings.mode;
  }
  // lib.optionalAttrs enabled (
    {
      inherit (settings) sender region;
      configuration_set = settings.configurationSet;
      credential_file = "${runtimeDir}/credentials/mail-ses";
      control_signing_key_file = "${runtimeDir}/credentials/mail-controls";
      max_campaign_recipients = settings.maxCampaignRecipients;
      max_daily_messages = settings.maxDailyMessages;
      max_daily_confirmation_messages = settings.maxDailyConfirmationMessages;
      send_interval_milliseconds = settings.sendIntervalMilliseconds;
    }
    // lib.optionalAttrs (subscriptions != null) {
      subscriptions = {
        inherit (subscriptions) mode purpose;
        operator_name = subscriptions.operatorName;
        postal_address = subscriptions.postalAddress;
        privacy_url = subscriptions.privacyUrl;
        contact_address = subscriptions.contactAddress;
      };
    }
    // lib.optionalAttrs (feedback != null) {
      feedback = {
        queue_url = feedback.queueUrl;
        topic_arn = feedback.topicArn;
      };
    }
  );

  credentials = lib.mapAttrsToList (
    name: path: "${name}:${if path == null then "/missing-${name}" else path}"
  ) protectedFiles;
  prepareCredentials = lib.concatStringsSep "\n" (
    lib.mapAttrsToList (name: _: ''
      ${pkgs.coreutils}/bin/install -m 0600 "$CREDENTIALS_DIRECTORY/${name}" ${lib.escapeShellArg "${runtimeDir}/credentials/${name}"}
    '') protectedFiles
  );
}
