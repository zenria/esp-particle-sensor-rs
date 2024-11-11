use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Result};
use embedded_hal::delay::DelayNs;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::AnyIOPin;
use esp_idf_svc::hal::prelude::Peripherals;
use esp_idf_svc::hal::reset::restart;
use esp_idf_svc::hal::uart::{self, UartDriver};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::{EspIOError, Write};
use esp_idf_svc::mqtt::client::{EspMqttClient, MqttClientConfiguration};
use macaddr::MacAddr;
use rgb_led::{RGB8, WS2812RMT};
use sds011::{Measurement, SDS011};
use wifi::wifi;

mod rgb_led;
mod wifi;

/// This configuration is picked up at compile time by `build.rs` from the
/// file `cfg.toml`.
#[toml_cfg::toml_config]
pub struct Config {
    #[default("Wokwi-GUEST")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_psk: &'static str,
    #[default("")]
    mqtt_broker_url: &'static str,
}

const BLUE: RGB8 = RGB8::new(0, 0, 50);
const GREEN: RGB8 = RGB8::new(0, 50, 0);
const BLACK: RGB8 = RGB8::new(0, 0, 0);

struct Delay;

impl DelayNs for Delay {
    fn delay_ns(&mut self, n: u32) {
        std::thread::sleep(Duration::from_nanos(n.into()));
    }
}
fn main() {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();
    loop {
        log::info!("starting app!");
        if let Err(e) = do_main() {
            log::error!("Error in do_main {e:?}");
            std::thread::sleep(Duration::from_secs(1));
            restart();
        }
    }
}

enum Message {
    Blink,
    NewMeasurement,
}

fn do_main() -> Result<()> {
    let peripherals = Peripherals::take().unwrap();
    let sysloop = EspSystemEventLoop::take()?;

    log::info!("Hello, world!");

    let mut led = WS2812RMT::new(peripherals.pins.gpio8, peripherals.rmt.channel0)?;
    led.set_pixel(RGB8::new(100, 0, 0))?;

    // The constant `CONFIG` is auto-generated by `toml_config`.
    let app_config = CONFIG;

    let config = uart::config::Config::default()
        .baudrate(Hertz(9600))
        .stop_bits(uart::config::StopBits::STOP1)
        .parity_none()
        .data_bits(uart::config::DataBits::DataBits8);
    let uart = UartDriver::new(
        peripherals.uart1,
        peripherals.pins.gpio0,
        peripherals.pins.gpio1,
        Option::<AnyIOPin>::None,
        Option::<AnyIOPin>::None,
        &config,
    )?;

    let sds011 = SDS011::new(uart, sds011::Config::default());
    let mut sds011 = sds011.init(&mut Delay)?;
    let fw = sds011.version();
    let id = sds011.id();
    log::info!("SDS011/021, ID: {id}, Firmware: {fw}");

    let particles_measurement = Arc::new(Mutex::new(Option::<Measurement>::None));

    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::spawn({
        let particles_measurement = particles_measurement.clone();
        let tx = tx.clone();
        move || loop {
            match sds011.measure(&mut Delay) {
                Ok(vals) => {
                    log::info!("Particle sensors measured: {vals}");
                    *particles_measurement.lock().unwrap() = Some(vals);
                    let _ = tx.send(Message::NewMeasurement);
                }
                Err(e) => log::error!("Unable to measure particles: {e:?}"),
            }
            // wait for 5-min
            std::thread::sleep(Duration::from_secs(5 * 60));
        }
    });

    led.set_pixel(RGB8::new(255, 100, 0))?;

    // Connect to the Wi-Fi network
    let wifi = match wifi(
        app_config.wifi_ssid,
        app_config.wifi_psk,
        peripherals.modem,
        sysloop,
    ) {
        Ok(inner) => inner,
        Err(err) => {
            // Red!
            led.set_pixel(RGB8::new(50, 0, 0))?;
            bail!("Could not connect to Wi-Fi network: {:?}", err)
        }
    };
    let mac_addr = MacAddr::from(wifi.get_mac(esp_idf_svc::wifi::WifiDeviceId::Sta)?);
    let root_topic = format!("esp32/{mac_addr}");

    // Set the HTTP server
    let mut server = EspHttpServer::new(&Configuration::default())?;
    // http://<sta ip>/ handler
    //let tx = Arc::new(tx);

    server.fn_handler("/", Method::Get, {
        let particles_measurement = particles_measurement.clone();
        move |request| -> core::result::Result<(), EspIOError> {
            let particles_measurement = particles_measurement.lock().unwrap();
            let html = templated(match particles_measurement.as_ref() {
                Some(vals) => format!("{vals}"),
                None => "No measure".to_string(),
            });
            let mut response = request.into_ok_response()?;
            response.write_all(html.as_bytes())?;
            Ok(())
        }
    })?;
    log::info!("HTTP Server awaiting connection");

    let mqtt_config = MqttClientConfiguration::default();
    let mut client = EspMqttClient::new_cb(
        app_config.mqtt_broker_url,
        &mqtt_config,
        move |_message_event| {
            // ... your handler code here - leave this empty for now
            // we'll add functionality later in this chapter
        },
    )?;
    log::info!("MQTT client created, root topic {root_topic}");

    thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));
        let _ = tx.send(Message::Blink);
    });

    // Green!
    led.set_pixel(RGB8::new(0, 50, 0))?;
    // Wait...
    std::thread::sleep(std::time::Duration::from_secs(1));
    led.set_pixel(RGB8::new(0, 0, 0))?;
    loop {
        match rx.recv() {
            Ok(message) => match message {
                Message::Blink => {
                    led.set_pixel(GREEN)?;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    led.set_pixel(BLUE)?;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    led.set_pixel(BLACK)?;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Message::NewMeasurement => {
                    log::debug!("NEW MEASUREMENT");
                    let particles_measurement = particles_measurement.lock().unwrap();
                    if let Some(vals) = particles_measurement.as_ref() {
                        log::debug!("publishing measures");
                        client.publish(
                            &format!("{root_topic}/PM25"),
                            esp_idf_svc::mqtt::client::QoS::AtLeastOnce,
                            true,
                            format!("{}", vals.pm25() as f32 / 10.0).as_bytes(),
                        )?;
                        client.publish(
                            &format!("{root_topic}/PM10"),
                            esp_idf_svc::mqtt::client::QoS::AtLeastOnce,
                            true,
                            format!("{}", vals.pm10() as f32 / 10.0).as_bytes(),
                        )?;
                    }
                }
            },
            Err(_) => log::error!("Unable to read channel"),
        }
    }
}

fn templated(content: impl AsRef<str>) -> String {
    format!(
        r#"
<!DOCTYPE html>
<html>
    <head>
        <meta charset="utf-8">
        <title>esp-rs web server</title>
    </head>
    <body>
        {}
    </body>
</html>
"#,
        content.as_ref()
    )
}
