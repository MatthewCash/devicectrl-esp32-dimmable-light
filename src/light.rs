use defmt::{error, info, warn};
use devicectrl_common::{
    DeviceId, DeviceState,
    device_types::dimmable_light::DimmableLightState,
    protocol::simple::{
        DeviceBoundSimpleMessage, ServerBoundSimpleMessage,
        esp::{TransportChannels, TransportEvent},
    },
    updates::AttributeUpdate,
};
use esp_hal::ledc::{
    LowSpeed,
    channel::{Channel, ChannelIFace},
};

use crate::log_error;

fn build_state(current_brightness: u8) -> DeviceState {
    DeviceState::DimmableLight(DimmableLightState {
        power: current_brightness > 0,
        brightness: current_brightness,
    })
}

#[embassy_executor::task]
pub async fn app_task(
    led_channel: &'static mut Channel<'static, LowSpeed>,
    transport: &'static TransportChannels,
) {
    let mut current_brightness = 0u8;
    loop {
        match transport.incoming.receive().await {
            TransportEvent::Connected => {
                info!("Connected to server!");

                // This isn't required, but its nice to tell the server our initial state
                transport
                    .outgoing
                    .send(ServerBoundSimpleMessage::UpdateNotification(
                        devicectrl_common::UpdateNotification {
                            device_id: DeviceId::from(crate::DEVICE_ID).unwrap(),
                            reachable: true,
                            new_state: build_state(current_brightness),
                        },
                    ))
                    .await;
            }
            TransportEvent::Error(err) => {
                log_error(&err);
            }
            TransportEvent::Message(DeviceBoundSimpleMessage::UpdateCommand(update)) => {
                if update.device_id.as_str() != crate::DEVICE_ID {
                    warn!(
                        "Received update command for different device {}!",
                        update.device_id.as_str()
                    );
                    continue;
                }

                let new_brightness = match update.update {
                    AttributeUpdate::Power(update) => {
                        if update.power {
                            100
                        } else {
                            0
                        }
                    }
                    AttributeUpdate::Brightness(update) => update.brightness,

                    _ => {
                        warn!("Requested state is not a dimmable light state!");
                        continue;
                    }
                };

                info!("Setting light brightness to [{}]", new_brightness);

                if let Err(err) = led_channel.set_duty(new_brightness) {
                    error!("Failed to set duty cycle: {:?}", err);
                } else {
                    current_brightness = new_brightness;
                }

                transport
                    .outgoing
                    .send(ServerBoundSimpleMessage::UpdateNotification(
                        devicectrl_common::UpdateNotification {
                            device_id: DeviceId::from(crate::DEVICE_ID).unwrap(),
                            reachable: true,
                            new_state: build_state(current_brightness),
                        },
                    ))
                    .await;
            }
            TransportEvent::Message(DeviceBoundSimpleMessage::StateQuery { device_id }) => {
                if device_id.as_str() != crate::DEVICE_ID {
                    warn!(
                        "Received state query for different device {}!",
                        device_id.as_str()
                    );
                    continue;
                }

                transport
                    .outgoing
                    .send(ServerBoundSimpleMessage::UpdateNotification(
                        devicectrl_common::UpdateNotification {
                            device_id,
                            reachable: true,
                            new_state: build_state(current_brightness),
                        },
                    ))
                    .await;
            }
            _ => {}
        }
    }
}
