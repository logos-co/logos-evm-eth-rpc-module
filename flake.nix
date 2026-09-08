{
  description = "Logos eth_rpc module — proxyable, fail-closed Ethereum JSON-RPC client (per-chain config, socks5h/Tor-ready).";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";

    # Declared OPTIONAL in metadata.json: typed, but never loaded, bundled or built. It
    # contributes only its published `packages.<system>.lidl` -- a single ~20 KB contract --
    # so libverifproxy and the nimbus closure stay out of every consumer of this module. The
    # follows keeps the generated ABI on one module-builder; without it the dependency drags
    # its own and the skew segfaults inside provider init.
    modules_state = {
      url = "github:logos-co/logos-modules-state-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    verified_proxy_module = {
      url = "github:logos-co/logos-verified-proxy-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
  };

  outputs = inputs@{ self, logos-module-builder, ... }:
    let
      nixpkgs = logos-module-builder.inputs.nixpkgs;
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;
    in
    {
      packages = forAllSystems (system:
        (logos-module-builder.lib.mkLogosModule {
          src = ./.;
          configFile = ./metadata.json;
          flakeInputs = inputs;
        }).packages.${system});
    };
}
