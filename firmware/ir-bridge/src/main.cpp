// Wi-Fi to IR bridge for homeremote.
//
// The Samsung UN55J6203 has no IP control at all (see the README), so this
// stands in front of it: homeremote asks over HTTP, and this blasts IR.
//
// It sends Samsung's *discrete* power codes rather than the power toggle on the
// physical remote. A toggle sent to a TV that is already on turns it off, and
// IR gives no feedback to tell the two apart — so "Watch Apple TV" could switch
// the TV off. Discrete codes make every request safe to repeat.
//
//   GET  /            status, as JSON
//   POST /power/on    discrete power on
//   POST /power/off   discrete power off
//   POST /samsung?code=0xE0E040BF   any Samsung code, for trying new ones
//   POST /ir/test     3 s of unbroken carrier, to check the LED with a camera

#include <Arduino.h>
#include <ESPmDNS.h>
#include <IRsend.h>
#include <WebServer.h>
#include <WiFi.h>
#include <esp_system.h>

#include "secrets.h"

// ATOM Lite and ATOM Matrix share these pins.
constexpr uint8_t IR_PIN = 12;
constexpr uint8_t LED_PIN = 27;

constexpr uint16_t HTTP_PORT = 80;
// What homeremote browses for. Changing it orphans every installed bridge.
constexpr const char* MDNS_SERVICE = "_homeremote-ir";

constexpr uint64_t SAMSUNG_POWER_ON = 0xE0E09966;
constexpr uint64_t SAMSUNG_POWER_OFF = 0xE0E019E6;
// 20 frames, about two seconds of IR. One frame is ~70 ms, and the ATOM's
// LED is weak: a single frame only landed with the bridge aimed carefully.
// Repeating is harmless because the codes are discrete — "on" twenty times is
// still just "on".
constexpr uint16_t SAMSUNG_REPEATS = 19;

// Reconnecting in place is unreliable on the ESP32 and leaves mDNS
// unannounced, so after this long offline the bridge starts over.
constexpr uint32_t OFFLINE_RESTART_MS = 60 * 1000;

IRsend ir(IR_PIN);
WebServer server(HTTP_PORT);

String bridgeId;  // the MAC, lower case with colons: homeremote's identity
String hostName;  // homeremote-ir-12bf08, from the last half of the MAC
uint32_t codesSent = 0;
uint32_t offlineSince = 0;

// Off, because the white flash on each send drowns out the IR LED when you
// check it with a phone camera — the only way to see whether IR is going out.
constexpr bool STATUS_LED = false;

void led(uint8_t r, uint8_t g, uint8_t b) {
  // The SK6812 is blinding at full power; these values are already dimmed.
  if (!STATUS_LED) r = g = b = 0;
  neopixelWrite(LED_PIN, r, g, b);
}

void blast(uint64_t code) {
  led(40, 40, 40);
  ir.sendSAMSUNG(code, kSamsungBits, SAMSUNG_REPEATS);
  codesSent++;
  Serial.printf("sent samsung 0x%08llX\n", code);
  led(0, 12, 0);
}

// Sent before blasting: the frames take two seconds, and homeremote should not
// wait on them — nor time out on them, at its three-second limit.
void replyOk(const char* action) {
  server.send(200, "application/json", String("{\"ok\":true,\"sent\":\"") + action + "\"}");
}

void handleStatus() {
  String body = "{";
  body += "\"id\":\"" + bridgeId + "\",";
  body += "\"model\":\"atom\",";
  body += "\"target\":\"samsung\",";
  body += "\"ip\":\"" + WiFi.localIP().toString() + "\",";
  body += "\"rssi\":" + String(WiFi.RSSI()) + ",";
  body += "\"uptime_s\":" + String(millis() / 1000) + ",";
  body += "\"codes_sent\":" + String(codesSent);
  body += "}";
  server.send(200, "application/json", body);
}

void handlePowerOn() {
  replyOk("power-on");
  blast(SAMSUNG_POWER_ON);
}

void handlePowerOff() {
  replyOk("power-off");
  blast(SAMSUNG_POWER_OFF);
}

// A code is ~70 ms of light, too brief to catch on a phone screen. This holds
// the 38 kHz carrier for three seconds — like holding a remote button down,
// so it is within what the LED is built for, and the TV ignores it.
void handleIrTest() {
  ir.enableIROut(38000);
  for (int i = 0; i < 60; i++) ir.mark(50000);
  ir.space(0);
  Serial.println("held carrier for 3 s");
  server.send(200, "application/json", "{\"ok\":true,\"sent\":\"ir-test\"}");
}

void handleSamsung() {
  String arg = server.arg("code");
  char* end = nullptr;
  uint64_t code = strtoull(arg.c_str(), &end, 0);
  if (arg.isEmpty() || *end != '\0' || code > 0xFFFFFFFFULL) {
    server.send(400, "application/json",
                "{\"ok\":false,\"error\":\"code must be a 32-bit number, e.g. 0xE0E040BF\"}");
    return;
  }
  replyOk("samsung");
  blast(code);
}

void connectWifi() {
  led(0, 0, 30);
  WiFi.mode(WIFI_STA);
  // Without this the radio dozes between beacons, and a request can sit for
  // hundreds of milliseconds before the bridge even hears it.
  WiFi.setSleep(false);
  WiFi.setHostname(hostName.c_str());
  WiFi.begin(WIFI_SSID, WIFI_PASSWORD);

  Serial.printf("joining %s", WIFI_SSID);
  uint32_t started = millis();
  while (WiFi.status() != WL_CONNECTED) {
    if (millis() - started > OFFLINE_RESTART_MS) {
      Serial.println("\nno network; restarting");
      ESP.restart();
    }
    delay(250);
    Serial.print('.');
  }
  Serial.printf("\nconnected as %s\n", WiFi.localIP().toString().c_str());
}

void advertise() {
  if (!MDNS.begin(hostName.c_str())) {
    Serial.println("mDNS failed to start; homeremote will not find this bridge");
    return;
  }
  MDNS.addService(MDNS_SERVICE, "_tcp", HTTP_PORT);
  // The MAC is the identity, so a new DHCP lease is invisible to homeremote.
  MDNS.addServiceTxt(MDNS_SERVICE, "_tcp", "id", bridgeId);
  MDNS.addServiceTxt(MDNS_SERVICE, "_tcp", "model", "atom");
  MDNS.addServiceTxt(MDNS_SERVICE, "_tcp", "target", "samsung");
  Serial.printf("advertising %s._tcp as %s.local\n", MDNS_SERVICE, hostName.c_str());
}

void setup() {
  Serial.begin(115200);
  led(0, 0, 0);
  ir.begin();

  // Read from eFuse: WiFi.macAddress() is all zeros until the radio starts.
  uint8_t mac[6];
  esp_read_mac(mac, ESP_MAC_WIFI_STA);
  char text[18];
  snprintf(text, sizeof text, "%02x:%02x:%02x:%02x:%02x:%02x",
           mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
  bridgeId = text;
  snprintf(text, sizeof text, "%02x%02x%02x", mac[3], mac[4], mac[5]);
  hostName = String("homeremote-ir-") + text;

  connectWifi();
  advertise();

  server.on("/", HTTP_GET, handleStatus);
  server.on("/power/on", HTTP_POST, handlePowerOn);
  server.on("/power/off", HTTP_POST, handlePowerOff);
  server.on("/samsung", HTTP_POST, handleSamsung);
  server.on("/ir/test", HTTP_POST, handleIrTest);
  server.onNotFound([] {
    server.send(404, "application/json", "{\"ok\":false,\"error\":\"not found\"}");
  });
  server.begin();

  led(0, 12, 0);
  Serial.printf("ready: http://%s/\n", WiFi.localIP().toString().c_str());
}

void loop() {
  if (WiFi.status() != WL_CONNECTED) {
    if (offlineSince == 0) {
      offlineSince = millis();
      led(30, 0, 0);
      Serial.println("lost the network");
    } else if (millis() - offlineSince > OFFLINE_RESTART_MS) {
      ESP.restart();
    }
  } else if (offlineSince != 0) {
    offlineSince = 0;
    led(0, 12, 0);
  }
  server.handleClient();
  delay(2);
}
