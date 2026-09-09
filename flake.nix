{
  description = "Logos eth_rpc module — proxyable, fail-closed Ethereum JSON-RPC client (per-chain config, socks5h/Tor-ready).";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";

    # Declared OPTIONAL in metadata.json: typed, and tolerated when absent. Each contributes
    # its published `packages.<system>.lidl` -- verified_proxy_module's is a single
    # 19,928-byte contract.
    #
    # A REQUIRED declaration would not pull libverifproxy in either: measured, the closure holds
    # 0 nimbus paths and `.#install` stages only this module either way. `optional` is about
    # absence being tolerated, not about the closure.
    #
    # The follows is for LOCK SIZE, not compatibility: without it each dependency drags its own
    # module-builder subtree and this lock goes 756 -> 2250 nodes. The contract is unaffected
    # either way -- measured byte-identical with and without.
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
