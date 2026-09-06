{
  buildGoModule,
  fetchFromGitHub,
  lib,
}:
buildGoModule (finalAttrs: {
  pname = "maincopy-litestream";
  version = "0.5.17";
  src = fetchFromGitHub {
    owner = "benbjohnson";
    repo = "litestream";
    rev = "v${finalAttrs.version}";
    hash = "sha256-NOSyBKmxy+gtLFl4XgmU4xkKT06yRhEEdfcM0mB7ajU=";
  };
  vendorHash = "sha256-IbnLypkKqtm+wceNXakdeML66fHNmuBRi+cWSFmUKWk=";
  subPackages = [ "cmd/litestream" ];
  ldflags = [
    "-s"
    "-w"
    "-X main.Version=${finalAttrs.version}"
  ];
  meta = {
    description = "Upstream Litestream with the SQLite WAL-reset fix required by Maincopy";
    homepage = "https://litestream.io/";
    license = lib.licenses.asl20;
    mainProgram = "litestream";
  };
})
