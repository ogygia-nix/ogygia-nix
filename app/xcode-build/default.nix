{ lib
, stdenvNoCC
, fetchurl
, python3
, unzip
, writeText
, xcode
}:

let
  # Xcode resolves Swift packages over the network, which the sandbox
  # forbids. Instead, read the pins Xcode recorded in Package.resolved and
  # lay the packages out the way a resolve leaves them, so xcodebuild finds
  # everything already in place and never fetches.
  #
  # Binary targets are downloaded from URLs that each package declares in
  # its own way, so the caller describes them per package identity, as a
  # function from the package's source to its targets.
  swiftPackages = { resolved, binaryTargets ? { } }:
    map
      (pin:
        let
          src = builtins.fetchGit {
            url = pin.location;
            rev = pin.state.revision;
            allRefs = true;
            shallow = true;
          };
        in
        {
          inherit pin src;
          name = lib.removeSuffix ".git" (baseNameOf pin.location);
          packageRef = {
            inherit (pin) identity kind location;
            name = lib.removeSuffix ".git" (baseNameOf pin.location);
          };
          artifacts = map
            (target: target // {
              # SwiftPM checksums are the SHA-256 of the archive, which is
              # exactly what fetchurl verifies.
              archive = fetchurl { inherit (target) url; sha256 = target.checksum; };
            })
            ((binaryTargets.${pin.identity} or (_: [ ])) src);
        })
      (lib.importJSON resolved).pins;

  # SwiftPM's workspace state (format 7, as written by Xcode 27), with
  # @SOURCE_PACKAGES@ standing in for the build-time directory.
  workspaceState = packages: writeText "workspace-state.json" (builtins.toJSON {
    version = 7;
    object = {
      dependencies = map
        (p: {
          inherit (p) packageRef;
          state = {
            name = "sourceControlCheckout";
            # Absent fields are omitted, as Xcode writes them.
            checkoutState = lib.filterAttrs (_: v: v != null) {
              inherit (p.pin.state) revision;
              version = p.pin.state.version or null;
              branch = p.pin.state.branch or null;
            };
          };
          subpath = p.name;
          # SwiftPM requires the key, and discards the whole state without it.
          basedOn = null;
        })
        packages;
      artifacts = lib.concatMap
        (p: map
          (a: {
            inherit (p) packageRef;
            inherit (a) targetName;
            source = { type = "remote"; inherit (a) url checksum; };
            path = "@SOURCE_PACKAGES@/artifacts/${p.pin.identity}/${a.targetName}/${a.targetName}.xcframework";
            kind.xcframework = { };
          })
          p.artifacts)
        packages;
      prebuilts = [ ];
    };
  });

  layOutPackages = lib.concatMapStrings
    (p: ''
      cp -R ${p.src} "$sourcePackages/checkouts/${p.name}"
    '' + lib.concatMapStrings
      (a: ''
        mkdir -p "$sourcePackages/artifacts/${p.pin.identity}/${a.targetName}"
        unzip -q ${a.archive} -d "$sourcePackages/artifacts/${p.pin.identity}/${a.targetName}"
      '')
      p.artifacts);

  # Builds one scheme of an Xcode project with xcodebuild, inside the
  # sandbox and unsigned; the caller's installPhase copies the products out
  # of $derivedData. Signing happens after the build, outside the sandbox.
  #
  # The bundle keeps the project's placeholder version, build number and git
  # revision, so the build depends only on the app's own inputs and stays the
  # same derivation across commits that leave them alone. stampXcodeApp sets
  # the real values afterwards.
  buildXcodeApp =
    { project
    , scheme
    , packages ? [ ]
    , xcodebuildFlags ? [ ]
    , nativeBuildInputs ? [ ]
    , passthru ? { }
    , ...
    }@args:
    stdenvNoCC.mkDerivation (builtins.removeAttrs args [ "project" "scheme" "packages" "xcodebuildFlags" ] // {
      version = "unstamped";
      nativeBuildInputs = nativeBuildInputs ++ lib.optional (packages != [ ]) unzip;

      # Host paths xcodebuild cannot run without. They are readable only, and
      # the sandbox still has no network:
      # - /System/Library and /usr/lib: system frameworks and libraries.
      # - /usr/share/firmlinks: FSEvents maps paths between the system and data
      #   volumes with it; without it Xcode's file watcher gets no paths back
      #   and xcodebuild crashes.
      # - /Library/Developer/PrivateFrameworks: CoreSimulator and friends,
      #   installed by `xcodebuild -runFirstLaunch` and loaded by Xcode from
      #   that absolute path.
      # - /Library/Apple/System/Library/PrivateFrameworks: MobileDevice and
      #   friends, also installed at first launch, which CoreDevice loads
      #   through symlinks in /System/Library/PrivateFrameworks.
      # - /Library/Preferences/com.apple.dt.Xcode.plist: records acceptance of
      #   the Xcode licence, which xcodebuild checks.
      # - /usr/bin/codesign: Xcode verifies the signature of binary
      #   xcframeworks such as Sparkle's before linking them.
      # - /usr/bin/touch: the build system stamps bundles with it.
      # Builders must list these in allowed-impure-host-deps.
      __impureHostDeps = [
        "/System/Library"
        "/usr/lib"
        "/usr/share/firmlinks"
        "/Library/Developer/PrivateFrameworks"
        "/Library/Apple/System/Library/PrivateFrameworks"
        "/Library/Preferences/com.apple.dt.Xcode.plist"
        "/usr/bin/codesign"
        "/usr/bin/touch"
      ];

      # The apps build against the runner's macOS through the host deps
      # above, which the derivation does not otherwise record. Naming the
      # runner makes moving the apps to another runner a change to them.
      runner = lib.fileContents ../../.github/macos-apps-runner;

      configurePhase = ''
        runHook preConfigure

        # Foundation takes the home directory from the build user's account,
        # /var/empty, unless CFFIXED_USER_HOME overrides it.
        export HOME="$TMPDIR/home"
        export CFFIXED_USER_HOME="$HOME"
        mkdir -p "$HOME"
        # SwiftPM compiles package manifests without HOME, so the clang module
        # cache would otherwise land in /var/empty too.
        export XDG_CACHE_HOME="$TMPDIR/cache"
        export SWIFTPM_MODULECACHE_OVERRIDE="$TMPDIR/cache/ModuleCache"
        export DEVELOPER_DIR=${xcode}/Contents/Developer
        export PATH="$DEVELOPER_DIR/usr/bin:$PATH"

        derivedData="$TMPDIR/DerivedData"
        sourcePackages="$TMPDIR/SourcePackages"
        mkdir -p "$sourcePackages/checkouts" "$sourcePackages/artifacts"
        ${layOutPackages packages}
        chmod -R u+w "$sourcePackages"
        sed "s|@SOURCE_PACKAGES@|$sourcePackages|g" ${workspaceState packages} > "$sourcePackages/workspace-state.json"

        runHook postConfigure
      '';

      buildPhase = ''
        runHook preBuild

        # The linker's ad-hoc signature is enough to run the result locally.
        #
        # A process inside the Nix sandbox cannot apply a sandbox of its own,
        # so SwiftPM's manifest sandbox and the compiler's macro plugin sandbox
        # are turned off; both still run inside this one.
        xcodebuild \
          -project ${project} \
          -scheme ${scheme} \
          -configuration Release \
          ${lib.escapeShellArgs xcodebuildFlags} \
          -derivedDataPath "$derivedData" \
          -clonedSourcePackagesDirPath "$sourcePackages" \
          -disableAutomaticPackageResolution \
          -onlyUsePackageVersionsFromResolvedFile \
          -skipPackagePluginValidation \
          -skipMacroValidation \
          -IDEPackageSupportDisableManifestSandbox=YES \
          build \
          CODE_SIGNING_ALLOWED=NO \
          OTHER_SWIFT_FLAGS='$(inherited) -disable-sandbox'

        runHook postBuild
      '';

      # The apps run on devices without these store paths, so they may refer
      # to none. In particular, Xcode's licence forbids redistributing it, so
      # an app must never retain a reference that would carry Xcode into a
      # binary cache with it.
      allowedReferences = [ ];

      passthru = { inherit xcode; } // passthru;

      meta = {
        license = lib.licenses.mit;
        platforms = [ "aarch64-darwin" ];
      } // args.meta or { };
    });

  # Sets the version, build number and git revision of an app that
  # buildXcodeApp built. They live only in Info.plist, so this needs no Xcode
  # and takes seconds. Nothing in the bundle is sealed yet: xcodebuild signs
  # nothing, and the linker's ad-hoc signature of an executable does not bind
  # Info.plist.
  stampXcodeApp =
    { app
    , infoPlist
    , version
    , buildNumber
    , gitRevision
    }:
    stdenvNoCC.mkDerivation {
      inherit (app) pname meta;
      inherit version;
      nativeBuildInputs = [ python3 ];

      dontUnpack = true;
      installPhase = ''
        runHook preInstall

        mkdir -p "$out/Applications"
        cp -R ${app}/Applications/Ogygia.app "$out/Applications/Ogygia.app"
        plist="$out/Applications/Ogygia.app/${infoPlist}"
        chmod u+w "$(dirname "$plist")" "$plist"
        python3 ${./stamp.py} "$plist" ${lib.escapeShellArgs [ version (toString buildNumber) gitRevision ]}

        runHook postInstall
      '';

      # Fixup would strip binaries the app ships already signed, such as
      # Sparkle's.
      dontFixup = true;
      allowedReferences = [ ];

      passthru = app.passthru // { unstamped = app; };
    };
in
{
  inherit swiftPackages buildXcodeApp stampXcodeApp;
}
