{
  UMask = "0077";
  NoNewPrivileges = true;
  PrivateTmp = true;
  PrivateDevices = true;
  PrivatePIDs = true;
  ProtectSystem = "strict";
  ProtectHome = true;
  ProtectKernelTunables = true;
  ProtectKernelModules = true;
  ProtectKernelLogs = true;
  ProtectControlGroups = true;
  ProtectClock = true;
  ProtectHostname = true;
  RestrictRealtime = true;
  RestrictSUIDSGID = true;
  RestrictNamespaces = true;
  LockPersonality = true;
  MemoryDenyWriteExecute = true;
  SystemCallArchitectures = "native";
  SystemCallFilter = [
    "@system-service"
    "~@debug"
    "~@mount"
    "~@privileged"
  ];
  RestrictAddressFamilies = [
    "AF_UNIX"
    "AF_INET"
    "AF_INET6"
  ];
  CapabilityBoundingSet = "";
  LimitCORE = 0;
}
