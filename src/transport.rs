use alloc::{vec, vec::Vec};
use anyhow::{Context, Result, anyhow, bail};
use core::{net::SocketAddrV4, str::FromStr};
use defmt::{error, info};
use devicectrl_common::{
    DeviceId, DeviceState, UpdateNotification,
    device_types::dimmable_light::DimmableLightState,
    protocol::simple::{DeviceBoundSimpleMessage, SIGNATURE_LEN, ServerBoundSimpleMessage},
    updates::AttributeUpdate,
};
use embassy_net::{Stack, tcp::TcpSocket};
use embassy_time::{Duration, Timer};
use embedded_io_async::Read;
use esp_hal::ledc::{
    LowSpeed,
    channel::{Channel, ChannelIFace},
};
use esp32_ecdsa::{CryptoContext, ecdsa_sign, ecdsa_verify};
use rand_core::RngCore;

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

    let mut expected_recv_nonce = crypto.trng.next_u32();
    socket
        .write(&expected_recv_nonce.to_be_bytes())
        .await
        .map_err(|err| anyhow!("failed to send client nonce: {:?}", err))?;

    let mut send_nonce_buf = [0u8; size_of::<u32>()];
    socket
        .read_exact(&mut send_nonce_buf)
        .await
        .map_err(|err| anyhow!("failed to read server nonce: {:?}", err))?;
    let mut send_nonce = u32::from_be_bytes(send_nonce_buf);

    send_identify_message(&mut socket).await?;

    info!("Connected to server!");

    loop {
        let mut nonce_buf = [0u8; size_of::<u32>()];
        socket
            .read_exact(&mut nonce_buf)
            .await
            .map_err(|err| anyhow!("failed to read server nonce: {:?}", err))?;
        let recv_nonce = u32::from_be_bytes(nonce_buf);

        expected_recv_nonce = expected_recv_nonce.wrapping_add(1);
        if recv_nonce != expected_recv_nonce {
            bail!(
                "Server nonce did not match expected value! expected={}, got={}",
                expected_recv_nonce,
                recv_nonce
            );
        }

        let mut len_buf = [0u8; size_of::<u32>()];
        socket
            .read_exact(&mut len_buf)
            .await
            .map_err(|err| anyhow!("failed to read length: {:?}", err))?;
        let payload_len = u32::from_be_bytes(len_buf) as usize;

        handle_message(
            &mut socket,
            &mut send_nonce,
            recv_nonce,
            payload_len,
            light_channel,
            crypto,
        )
        .await?;
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_message(
    socket: &mut TcpSocket<'_>,
    send_nonce: &mut u32,
    received_nonce: u32,
    payload_len: usize,
    light_channel: &mut Channel<'static, LowSpeed>,
    crypto: &mut CryptoContext<'_>,
) -> Result<()> {
    let mut payload = vec![0u8; payload_len];
    socket
        .read_exact(&mut payload)
        .await
        .map_err(|err| anyhow!("data recv: {:?}", err))?;

    let mut sig_buf = [0u8; SIGNATURE_LEN];
    socket
        .read_exact(&mut sig_buf)
        .await
        .map_err(|err| anyhow!("sig recv: {:?}", err))?;

    // Verify signature over [nonce | len | payload].
    let mut to_verify =
        Vec::with_capacity(size_of_val(send_nonce) + size_of::<u32>() + payload.len());
    to_verify.extend_from_slice(&received_nonce.to_be_bytes());
    to_verify.extend_from_slice(&(payload_len as u32).to_be_bytes());
    to_verify.extend_from_slice(&payload);

    if !ecdsa_verify(crypto, &to_verify, &sig_buf).context("ecdsa verification failed")? {
        bail!("signature does not match!");
    }

    let mut current_brightness = 0u8;

    let message: DeviceBoundSimpleMessage = serde_json::from_slice(&payload)?;
    match message {
        DeviceBoundSimpleMessage::UpdateCommand(update) => {
            if update.device_id.as_str() != DEVICE_ID {
                bail!("Update notification does not match this device id!")
            }

            update_state(light_channel, &update.update, &mut current_brightness)?;

            let state = query_state(current_brightness);
            send_state_update(socket, send_nonce, state, crypto).await?;
        }
        DeviceBoundSimpleMessage::StateQuery { device_id } => {
            if device_id.as_str() != DEVICE_ID {
                bail!("State query notification does not match this device id!")
            }

            let state = query_state(current_brightness);
            send_state_update(socket, send_nonce, state, crypto).await?;
        }
        _ => error!("Unknown command received!"),
    }

    Ok(())
}

fn update_state(
    light_channel: &mut Channel<'static, LowSpeed>,
    update: &AttributeUpdate,
    current_brightness: &mut u8,
) -> Result<()> {
    let new_brightness = match update {
        AttributeUpdate::Power(update) => {
            if update.power {
                100
            } else {
                0
            }
        }
        AttributeUpdate::Brightness(update) => update.brightness,
        _ => bail!("Requested state is not a dimmable light state!"),
    };

    info!("Setting light brightness to [{}]", new_brightness);

    if let Err(err) = light_channel.set_duty(new_brightness) {
        error!("Failed to set duty cycle: {:?}", err);
    } else {
        *current_brightness = new_brightness;
    }

    Ok(())
}

fn query_state(current_brightness: u8) -> DeviceState {
    DeviceState::DimmableLight(DimmableLightState {
        power: current_brightness > 0,
        brightness: current_brightness,
    })
}

async fn send_state_update(
    socket: &mut TcpSocket<'_>,
    send_nonce: &mut u32,
    state: DeviceState,
    crypto: &mut CryptoContext<'_>,
) -> Result<()> {
    let message = ServerBoundSimpleMessage::UpdateNotification(UpdateNotification {
        device_id: DeviceId::from(DEVICE_ID).map_err(|err| anyhow!(err))?,
        reachable: true,
        new_state: state,
    });

    send_message(socket, send_nonce, crypto, &message).await
}

async fn send_identify_message(socket: &mut TcpSocket<'_>) -> Result<()> {
    let data = serde_json::to_vec(&ServerBoundSimpleMessage::Identify(
        DeviceId::from(DEVICE_ID).map_err(|e| anyhow!(e))?,
    ))?;

    let len_be = (data.len() as u32).to_be_bytes();
    let mut framed = Vec::with_capacity(size_of::<u32>() + data.len());
    framed.extend_from_slice(&len_be);
    framed.extend_from_slice(&data);

    socket
        .write(&framed)
        .await
        .map_err(|err| anyhow!("{:?}", err))?;

    Ok(())
}

async fn send_message(
    socket: &mut TcpSocket<'_>,
    send_nonce: &mut u32,
    crypto: &mut CryptoContext<'_>,
    message: &ServerBoundSimpleMessage,
) -> Result<()> {
    let payload = serde_json::to_vec(message)?;
    let payload_len_be = (payload.len() as u32).to_be_bytes();

    *send_nonce = send_nonce.wrapping_add(1);
    let nonce_be = send_nonce.to_be_bytes();

    let mut to_sign =
        Vec::with_capacity(size_of_val(send_nonce) + size_of::<u32>() + payload.len());
    to_sign.extend_from_slice(&nonce_be);
    to_sign.extend_from_slice(&payload_len_be);
    to_sign.extend_from_slice(&payload);

    let sig = ecdsa_sign(crypto, &to_sign).context("ecdsa signing failed")?;

    let mut data =
        Vec::with_capacity(size_of_val(send_nonce) + size_of::<u32>() + payload.len() + sig.len());
    data.extend_from_slice(&nonce_be);
    data.extend_from_slice(&payload_len_be);
    data.extend_from_slice(&payload);
    data.extend_from_slice(&sig);

    socket
        .write(&data)
        .await
        .map_err(|err| anyhow!("{:?}", err))?;

    Ok(())
}
