use alloc::{vec, vec::Vec};
use anyhow::{Context, Result, anyhow, bail};
use core::{net::SocketAddrV4, str::FromStr};
use defmt::{error, info};
use devicectrl_common::{
    DeviceId, DeviceState, DeviceStateUpdate, UpdateNotification,
    device_types::led_strip::LedStripState,
    protocol::simple::{DeviceBoundSimpleMessage, SIGNATURE_LEN, ServerBoundSimpleMessage},
};
use embassy_net::{Stack, tcp::TcpSocket};
use embassy_time::{Duration, Timer};
use embedded_io_async::Read;
use esp_hal::ledc::{
    LowSpeed,
    channel::{Channel, ChannelIFace},
};

use crate::crypto::{CryptoContext, ecdsa_sign, ecdsa_verify};
use crate::{DEVICE_ID, log_error};

#[embassy_executor::task]
pub async fn connection_task(
    stack: &'static Stack<'static>,
    light_channel: &'static mut Channel<'static, LowSpeed>,
    mut crypto: CryptoContext<'static>,
) {
    loop {
        Timer::after(Duration::from_secs(5)).await;
        info!("Reconnecting to server...");

        if let Err(err) = open_connection(stack, light_channel, &mut crypto).await {
            log_error(&err.context("Failed to handle server loop"));
        }
    }
}

async fn open_connection(
    stack: &'static Stack<'_>,
    light_channel: &mut Channel<'static, LowSpeed>,
    crypto: &mut CryptoContext<'_>,
) -> Result<()> {
    let mut rx_buffer = [0u8; 4096];
    let mut tx_buffer = [0u8; 4096];

    let mut socket = TcpSocket::new(*stack, &mut rx_buffer, &mut tx_buffer);

    socket.set_keep_alive(Some(Duration::from_secs(60)));
    socket
        .connect(SocketAddrV4::from_str(env!("SERVER_ADDR")).expect("Invalid server address"))
        .await
        .map_err(|e| anyhow!("failed to connect: {:?}", e))?;

    send_identify_message(&mut socket).await?;

    info!("Connected to server!");

    loop {
        let mut len_buf = [0u8; size_of::<u32>()];
        if socket
            .read(&mut len_buf)
            .await
            .map_err(|err| anyhow!("size recv: {:?}", err))?
            != size_of::<u32>()
        {
            bail!("Length delimiter is not a u32!")
        }

        handle_message(
            &mut socket,
            u32::from_be_bytes(len_buf) as usize,
            light_channel,
            crypto,
        )
        .await?;
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_message(
    socket: &mut TcpSocket<'_>,
    message_len: usize,
    light_channel: &mut Channel<'static, LowSpeed>,
    crypto: &mut CryptoContext<'_>,
) -> Result<()> {
    let mut buf = vec![0u8; message_len];
    socket
        .read_exact(&mut buf)
        .await
        .map_err(|err| anyhow!("data recv: {:?}", err))?;

    let sig: &[u8; SIGNATURE_LEN] = &buf
        .get(..SIGNATURE_LEN)
        .context("message is not long enough for signature")?
        .try_into()?;

    let data = &buf
        .get(SIGNATURE_LEN..message_len)
        .context("message is not long enough")?;

    if !ecdsa_verify(crypto, data, sig).context("ecdsa verification failed")? {
        bail!("signature does not match!")
    }

    let mut current_brightness = 0u8;

    let message: DeviceBoundSimpleMessage = serde_json::from_slice(data)?;
    match message {
        DeviceBoundSimpleMessage::UpdateCommand(update) => {
            if update.device_id.as_str() != DEVICE_ID {
                bail!("Update notification does not match this device id!")
            }

            update_state(light_channel, &update.change_to, &mut current_brightness)?;

            let state = query_state(current_brightness);
            send_state_update(socket, state, crypto).await?;
        }
        DeviceBoundSimpleMessage::StateQuery { device_id } => {
            if device_id.as_str() != DEVICE_ID {
                bail!("State query notification does not match this device id!")
            }

            let state = query_state(current_brightness);
            send_state_update(socket, state, crypto).await?;
        }
        _ => error!("Unknown command received!"),
    }

    Ok(())
}

fn update_state(
    light_channel: &mut Channel<'static, LowSpeed>,
    requested_state: &DeviceStateUpdate,
    current_brightness: &mut u8,
) -> Result<()> {
    let DeviceStateUpdate::LedStrip(new_state) = requested_state else {
        bail!("Requested state is not a dimmable light state!")
    };

    let new_brightness = if new_state.power == Some(false) {
        Some(0)
    } else {
        new_state.brightness.min(Some(100))
    };

    if let Some(brightness) = new_brightness {
        info!("Setting light brightness to [{}]", brightness);

        if let Err(err) = light_channel.set_duty(brightness) {
            error!("Failed to set duty cycle: {:?}", err);
        } else {
            *current_brightness = brightness;
        }
    }

    Ok(())
}

fn query_state(current_brightness: u8) -> DeviceState {
    DeviceState::LedStrip(LedStripState {
        power: current_brightness > 0,
        brightness: current_brightness,
    })
}

async fn send_state_update(
    socket: &mut TcpSocket<'_>,
    state: DeviceState,
    crypto: &mut CryptoContext<'_>,
) -> Result<()> {
    let message = ServerBoundSimpleMessage::UpdateNotification(UpdateNotification {
        device_id: DeviceId::from(DEVICE_ID).map_err(|err| anyhow!(err))?,
        reachable: true,
        new_state: state,
    });

    send_message(socket, crypto, &message).await
}

async fn send_identify_message(socket: &mut TcpSocket<'_>) -> Result<()> {
    let mut data = serde_json::to_vec(&ServerBoundSimpleMessage::Identify(
        DeviceId::from(DEVICE_ID).map_err(|e| anyhow!(e))?,
    ))?;

    data.splice(0..0, data.len().to_be_bytes());

    socket
        .write(&data)
        .await
        .map_err(|err| anyhow!("{:?}", err))?;

    Ok(())
}

async fn send_message(
    socket: &mut TcpSocket<'_>,
    crypto: &mut CryptoContext<'_>,
    message: &ServerBoundSimpleMessage,
) -> Result<()> {
    let payload = serde_json::to_vec(message)?;
    let sig = ecdsa_sign(crypto, &payload).context("ecdsa signing failed")?;

    let total_len = (sig.len() + payload.len()) as u32;
    let mut data = Vec::with_capacity(size_of::<u32>() + total_len as usize);

    data.extend_from_slice(&total_len.to_be_bytes());
    data.extend_from_slice(&sig);
    data.extend_from_slice(&payload);

    socket
        .write(&data)
        .await
        .map_err(|err| anyhow!("{:?}", err))?;

    Ok(())
}
