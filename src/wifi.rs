use anyhow::{Result, anyhow};
use defmt::{error, info};
use embassy_time::{Duration, Timer};
use esp_wifi::{
    config::PowerSaveMode,
    wifi::{ClientConfiguration, Configuration, WifiController, WifiEvent, WifiState},
};

use crate::log_error;

#[embassy_executor::task]
pub async fn wifi_connection(mut controller: WifiController<'static>) {
    controller
        .set_power_saving(PowerSaveMode::None)
        .expect("Failed to disable wifi power saving");

    loop {
        if let Err(err) = run_wifi_loop(&mut controller).await {
            log_error(&err.context("Failed to handle wifi loop"));
        }
    }
}

async fn run_wifi_loop(controller: &mut WifiController<'static>) -> Result<()> {
    if esp_wifi::wifi::wifi_state() == WifiState::StaConnected {
        controller.wait_for_event(WifiEvent::StaDisconnected).await;
        Timer::after(Duration::from_millis(5000)).await
    }

    if !matches!(controller.is_started(), Ok(true)) {
        let client_config = Configuration::Client(ClientConfiguration {
            ssid: env!("WIFI_SSID")
                .try_into()
                .map_err(|err| anyhow!("{:?}", err))?,
            password: env!("WIFI_PASSWORD")
                .try_into()
                .map_err(|err| anyhow!("{:?}", err))?,
            ..Default::default()
        });

        controller
            .set_configuration(&client_config)
            .map_err(|err| anyhow!("{:?}", err))?;
        controller
            .start_async()
            .await
            .map_err(|err| anyhow!("{:?}", err))?;
        info!("Wifi started!");
    }

    match controller.connect_async().await {
        Ok(_) => info!("Wifi connected!"),
        Err(e) => {
            error!("Failed to connect to wifi: {:?}", e);
            Timer::after(Duration::from_millis(5000)).await
        }
    }

    Ok(())
}
