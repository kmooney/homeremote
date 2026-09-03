# Home Remote

A replacement for a Logitech Harmony whose IR repeaters fire unreliably, built
in Rust. Every device here is already on the network and IP-controllable, so
this removes IR — the only unreliable link — entirely.

Two pieces, as originally planned:

1. **`homeremote`** — a library that discovers and controls the devices over IP.
2. **`homeremote-daemon`** — a single binary that serves a React remote to the
   LAN, so any phone or laptop in the house is the remote.

## Quick start

```sh
cd ui && npm install && npm run build && cd ..   # build the UI into the binary
cargo build --release

./target/release/homeremote discover --save      # find the devices
./target/release/homeremote serve                # start the remote
```

`serve` prints the URLs to open. There is no login: it is a LAN appliance for
one household, the same trust model the receiver and the TV already use.

## Devices

### Denon AVR-S720W — working

Volume, power, mute, and input switching over the receiver's control port,
plus a live state stream.

The receiver **accepts exactly one connection on port 23** and refuses a second
outright. If control stops working, something else on the network has taken the
socket — an old Harmony hub, or Home Assistant. The daemon logs this specifically
rather than reporting a generic failure.

Volume moves in **half steps**: `MVUP` goes 57 → 57.5, not 57 → 58.

### Apple TV — discovery only

Found on the network; control is the remaining phase. The Companion protocol
needs PIN pairing plus HAP-style crypto and a hand-written OPACK codec, with
[pyatv](https://github.com/postlund/pyatv) as the reference.

Its mDNS record mostly **rotates for privacy** — `rpBA`, `rpAD`, `rpHI`, `rpHN`,
and `rpHA` all change within minutes. Identity is keyed on `rpMRtID`, which does
not, so pairing credentials will survive.

### Samsung UN55J6203 — no IP control available

A full scan of all 65535 ports, with the TV awake and answering, found exactly
three open:

| Port | Service | Useful? |
|------|---------|---------|
| 7676 | UPnP: DLNA, RenderingControl, AVTransport | volume/mute only, which the Denon already owns |
| 8080 | DIAL | launches apps like YouTube and Netflix |
| 7011 | Orsay Remote Web Inspector | bound, but resets on every HTTP request |

There is no key input, no input switching, and no power control.

The giveaway is port 7011: that is the **Orsay** Remote Web Inspector, so this
is a pre-Tizen set. Tizen's remote-control endpoints (`8002`, `/api/v2/`) were
never going to exist here — Orsay used port 55000 on older models and
`8001` + `/ms/1.0/` on 2014-15 ones, and neither is listening.

Two details worth recording, because they look like leads and are not:

- The TV **advertises** a MultiScreen API on port 8001 in its own UPnP
  capability list, but connecting there is *refused* — the service is
  advertised and not running. A cold boot is the usual remedy; it has not been
  tried here.
- Its `RemoteControlReceiver` UPnP device sounds like the answer but exposes
  only `MultiScreenService`, which launches apps. There is no key-input
  service behind it.

In standby the TV leaves the network completely — no ping, no open ports — so
every IP route is closed then too.

### HDMI-CEC, for TV power and input

The receiver ships with HDMI Control **off**. Its current setup reads:

```
SSHOSCON     ON     HDMI Control          (was OFF; now enabled)
SSHOSCONPOF  ALL    Power Off Control     receiver off -> connected devices off
SSHOSCONSTS  LAS    Standby Source        last used
SSHOSCONARC  OFF    Audio Return Channel
SSHOSPAS     ON     HDMI Pass Through
SSHOSTAS     OFF    TV Audio Switching
```

Two things worth knowing:

- **The receiver only accepts these settings while it is powered on.** Sending
  `SSHOSCON ON` in standby is accepted silently and does nothing, which is an
  easy hour to lose.
- **Powering the receiver on does not, by itself, wake the TV.** CEC "one touch
  play" is initiated by a *source* device, not by the amplifier in the middle.
  Cycling the receiver with HDMI Control enabled left the TV asleep and off the
  network for a full minute.

So the expected path to TV power is the **Apple TV**: waking it should send one
touch play, which turns the TV on and pulls both the TV and the receiver to the
right input. That makes finishing Companion pairing the prerequisite for TV
power, not a separate feature.

Still to confirm, and both need someone in the room:

- **Anynet+ (HDMI-CEC) must be enabled on the TV** — it is a TV menu setting and
  cannot be checked or changed over the network.
- Whether the TV was in standby or switched off at the wall during the test. It
  was unreachable throughout, so the CEC result is inconclusive rather than
  negative.

To change these settings yourself, the receiver takes raw protocol strings:

```sh
homeremote send denon raw SSHOS ?          # read the whole HDMI setup
homeremote send denon raw SSHOSCON ON      # enable  (receiver must be ON)
homeremote send denon raw SSHOSCON OFF     # revert
```

## Activities

Harmony-style macros, defined in the config file. `homeremote config` prints its
location.

```toml
[[activity]]
id = "apple-tv"
name = "Watch Apple TV"
volume_device = "denon:0005CD91E7A1"   # volume always goes here

  [[activity.steps]]
  device = "denon:0005CD91E7A1"
  command = { power = "on" }
  delay_ms = 2500          # waits AFTER the step

  [[activity.steps]]
  device = "denon:0005CD91E7A1"
  command = { input = "MPLAY" }
```

Two behaviours worth knowing:

- **`delay_ms` is load-bearing.** The receiver ignores input commands for a
  couple of seconds after power-on. Without the delay it works by hand and
  fails as a macro.
- **Steps already satisfied are skipped.** Switching between activities does not
  power-cycle equipment both of them use.

To find the input name your Apple TV is on: switch the receiver to it with its
own remote, then run `homeremote watch denon` and read the input it reports.

## CLI

The CLI exists so every protocol can be exercised against real hardware without
a UI in the way.

```sh
homeremote discover [--save]        # scan the network
homeremote send denon volume up     # also: power on|off, mute, input MPLAY
homeremote send denon volume 40%    # absolute level
homeremote send denon raw PSBAS UP  # any protocol string
homeremote watch denon              # follow state live, including the physical remote
homeremote config                   # show config path and contents
homeremote serve                    # run the daemon
```

Devices are matched by any unambiguous fragment of id, name, or vendor.

## How discovery works

Devices are keyed on a **stable identity, never an IP**, and re-resolved at every
startup so a new DHCP lease breaks nothing.

| Device | Identity | Found via |
|--------|----------|-----------|
| Denon | serial number (equals its MAC) | SSDP `upnp:rootdevice` + mDNS AirPlay, confirmed by manufacturer |
| Samsung | UPnP UDN | SSDP `RemoteControlReceiver` |
| Apple TV | `rpMRtID` | mDNS `_companion-link._tcp` |

Some notes that cost time to find:

- The AVR-S720W **ignores Denon's own SSDP target** and only answers the generic
  `upnp:rootdevice` search, so candidates are gathered widely and confirmed by
  the manufacturer field in each device description.
- The Samsung is identified by UPnP rather than its Tizen endpoint because it
  keeps serving UPnP in standby. Keying on an endpoint that only answers when
  the set is awake would give one TV two identities.
- Macs, iPhones, and HomePods all advertise Companion for Handoff. Only records
  carrying an `rpMd` of `AppleTV*` are treated as an Apple TV.
- SSDP sockets bind to `0.0.0.0` and select the interface with `IP_MULTICAST_IF`.
  Binding to the interface address makes macOS reject multicast sends with
  `EHOSTUNREACH`.

## Development

```sh
cargo test                 # library, protocol, and activity tests
cargo clippy --all-targets
cd ui && npm run dev       # UI with hot reload, proxying to a running daemon
```

Device backends sit behind cargo features (`denon`, `samsung`, `appletv`), so a
protocol that breaks cannot block a build.

## Status

- [x] Discovery for all three devices
- [x] Denon control and live state
- [x] Daemon, REST/WebSocket API, React remote, activities
- [ ] Apple TV over the Companion protocol
- [ ] Samsung — this model has no IP remote API; HDMI-CEC enabled on the receiver, awaiting confirmation that Anynet+ is on
