use anyhow::{Result, anyhow};
use defmt::{error, info};
use embassy_time::{Duration, Timer};
use esp_radio::wifi::{
    ClientConfig, ModeConfig, PowerSaveMode, WifiController, WifiEvent, WifiStaState,
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
    if esp_radio::wifi::sta_state() == WifiStaState::Connected {
        controller.wait_for_event(WifiEvent::StaDisconnected).await;
        Timer::after(Duration::from_millis(5000)).await
    }

    if !matches!(controller.is_started(), Ok(true)) {
        let client_config = ModeConfig::Client(
            ClientConfig::default()
                .with_ssid(env!("WIFI_SSID").into())
                .with_password(env!("WIFI_PASSWORD").into()),
        );

        controller
            .set_config(&client_config)
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
