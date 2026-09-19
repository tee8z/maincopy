# Integrate the Maincopy NixOS module

Maincopy owns its NixOS service definitions. A deployment repository pins the Maincopy flake, imports `nixosModules.default`, and supplies host settings.
Use the [deployment runbook](deployment.md) for owner initialization, certificates, and backups.

## Keep the integration small

Import the exported module instead of copying its service definitions:

```nix
{ inputs, ... }:
{
  imports = [ inputs.maincopy.nixosModules.default ];

  services.maincopy = {
    enable = true;
    public.domain = "www.example.com";
    admin.domain = "admin.example.com";
    contentRoot = "/srv/publication/content";
  };
}
```

The deployment flake must pass `inputs` through `specialArgs` and pin its dependencies in `flake.lock`.
This configuration keeps administration on loopback and requires an existing owner before Maincopy starts.
Provision the external checkout before startup, or select managed Git as described below.

| Maincopy module owns | Deployment repository owns |
| --- | --- |
| Package default and service commands | Pinned input revision and optional `services.maincopy.package` override |
| Service users, isolation, and generated configuration | Host hardware, operating system, and resource limits |
| Caddy gateways and route separation | DNS, listener addresses, firewall rules, and VPN access |
| Credential loading and private runtime copies | Original credential files and their provisioning |
| Database, managed Git mirror, and backup services | Persistent disks, mount dependencies, and backup destinations |
| Provided-certificate loading | Certificate issuance and gateway restart after renewal |

The same module supports `x86_64-linux` and `aarch64-linux` through the flake's package outputs.
A different machine changes deployment settings; it does not require a second Maincopy service implementation.

## Use an external content disk

In external checkout mode, `contentRoot` is read-only to the daemon. The host owns checkout creation and updates.
When the checkout uses a separate mount, require that mount before starting Maincopy:

```nix
{
  # Define fileSystems."/srv/publication" in the host's storage configuration.
  services.maincopy.contentRoot = "/srv/publication/content";

  systemd.services.maincopy = {
    unitConfig.RequiresMountsFor = [ "/srv/publication/content" ];
    bindsTo = [ "srv-publication.mount" ];
    after = [ "srv-publication.mount" ];
  };
}
```

Create the checkout after mounting its disk. Give the `maincopy` user read and directory traversal access.
Order any checkout preparation service before `maincopy.service` and require it from that service.
If you use `maincopy-initialize.service`, apply the same mount dependencies before running initialization.

The module stores database state under `/var/lib/<stateDirectory>` and its managed Git mirror beneath that directory.
Changing `contentRoot` does not move either location. Backups and gateway state also use separate directories under `/var/lib`.
To relocate application state, provide host mounts and order every affected service after those mounts.
Use the [backup restore procedure](backup-restore.md) when transferring existing state.

## Supply managed Git credentials

Managed Git already supports named runtime file references:

```nix
{
  services.maincopy.source = {
    managed = true;
    credentials.deploy = {
      privateKeyFile = "/var/lib/maincopy-credentials/source-key";
      knownHostsFile = "/etc/maincopy/source-known-hosts";
    };
  };
}
```

In managed mode, the module ignores `contentRoot` and keeps the mirror under its application state directory.
The module loads both files through systemd credentials and copies them into private runtime storage.
The host provisions the originals. Follow the [managed Git runbook](managed-source.md) for key ownership and verified host keys.
Keep private keys outside source control and the Nix store. Pass their paths as strings; do not read secret bytes into Nix expressions.

Configure the repository, branch, and content directory through Maincopy's source workflow. Those selections live in SQLite, not NixOS module options.

## Review a module update

Update the pinned Maincopy input in the deployment repository and inspect the resulting lockfile change.
Evaluate the host configuration before deployment. Preserve the module's service isolation settings when adding host dependencies.

Maincopy's own checks cover module evaluation, gateway configuration, backup operations, and a deployment virtual machine.
See [`flake.nix`](../flake.nix) and [`nix/tests/module-eval.nix`](../nix/tests/module-eval.nix) for the current checks and supported combinations.
