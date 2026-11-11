use std::time::Duration;

use bwk_qr::Progress;
use iced::{
    widget::{column, image, row, Space},
    Length, Subscription, Task,
};
use liana_ui::{
    component::{
        button, card, modal,
        qr::{self, Brightness},
        text::new::{b5_medium, caption},
    },
    spacing::{HSpacing, VSpacing},
    theme,
    widget::{modal::Modal, Container, Element, SpaceExt},
};

use crate::airgap::{
    request_camera_access, AnimatedQr, Answer, Ask, CameraDescriptor, CameraEvent, CameraFailure,
    CameraScanner, Error, Exchange, FRAMES_PER_SECOND,
};

/// The animation is advanced from the UI tick rather than a thread, so a closed
/// modal leaves nothing running. It has to tick faster than the frame rate to
/// land on each frame.
const ANIMATION_TICK: Duration = Duration::from_millis(50);
const CAMERA_POLL: Duration = Duration::from_millis(33);
const PREVIEW_HEIGHT: u32 = 300;

#[derive(Debug, Clone)]
pub enum AirgapAction {
    Brightness(Brightness),
    Enlarge(bool),
    Tick,
    Scan,
    Cameras(Result<Vec<CameraDescriptor>, CameraFailure>),
    SelectCamera(usize),
    PollCamera,
    Back,
    Cancel,
}

#[derive(Debug, Clone)]
pub enum AirgapOutcome {
    Answer(Box<Answer>),
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    ShowRequest,
    StartingCamera,
    Scanning,
    Done,
}

/// One QR exchange with an air-gapped signer, shared by every flow: Liana shows
/// the request as animated frames, then scans the signer's answer back.
pub struct AirgapModal {
    title: &'static str,
    exchange: Exchange,
    animation: Option<AnimatedQr>,
    phase: Phase,
    cameras: Vec<CameraDescriptor>,
    selected_camera: Option<usize>,
    scanner: Option<CameraScanner>,
    preview: Option<image::Handle>,
    brightness: Brightness,
    enlarged: bool,
    progress: Option<Progress>,
    error: Option<Error>,
    camera_error: Option<CameraFailure>,
    outcome: Option<AirgapOutcome>,
}

impl AirgapModal {
    pub fn new(title: &'static str, ask: Ask) -> Result<Self, Error> {
        let exchange = Exchange::new(ask, request_id())?;
        Ok(Self {
            title,
            animation: Some(AnimatedQr::new(
                exchange.frames().to_vec(),
                FRAMES_PER_SECOND,
            )?),
            exchange,
            phase: Phase::ShowRequest,
            cameras: Vec::new(),
            selected_camera: None,
            scanner: None,
            preview: None,
            brightness: Brightness::default(),
            enlarged: false,
            progress: None,
            error: None,
            camera_error: None,
            outcome: None,
        })
    }

    pub fn take_outcome(&mut self) -> Option<AirgapOutcome> {
        self.outcome.take()
    }

    pub fn subscription(&self) -> Subscription<AirgapAction> {
        let animation = if self.phase == Phase::ShowRequest {
            iced::time::every(ANIMATION_TICK).map(|_| AirgapAction::Tick)
        } else {
            Subscription::none()
        };
        let camera = if self.scanner.is_some() {
            iced::time::every(CAMERA_POLL).map(|_| AirgapAction::PollCamera)
        } else {
            Subscription::none()
        };
        Subscription::batch([animation, camera])
    }

    pub fn update(&mut self, message: AirgapAction) -> Task<AirgapAction> {
        match message {
            // the view reads the current frame straight off the animation
            AirgapAction::Tick => {}
            AirgapAction::Brightness(brightness) => self.brightness = brightness,
            AirgapAction::Enlarge(enlarged) => {
                self.enlarged = enlarged && self.phase == Phase::ShowRequest;
            }
            AirgapAction::Scan => {
                self.enlarged = false;
                self.stop_camera();
                self.phase = Phase::StartingCamera;
                self.clear_errors();
                return Task::perform(request_camera_access(), AirgapAction::Cameras);
            }
            AirgapAction::Cameras(result) => {
                // the permission callback may land after the modal moved on
                if self.phase != Phase::StartingCamera {
                    return Task::none();
                }
                match result {
                    Ok(cameras) if !cameras.is_empty() => {
                        self.cameras = cameras;
                        self.start_camera(0);
                    }
                    Ok(_) => self.give_up_on_camera(CameraFailure::Unavailable),
                    Err(failure) => self.give_up_on_camera(failure),
                }
            }
            AirgapAction::SelectCamera(index) => self.start_camera(index),
            AirgapAction::PollCamera => self.poll_camera(),
            AirgapAction::Back => {
                self.stop_camera();
                self.clear_errors();
                self.phase = Phase::ShowRequest;
            }
            AirgapAction::Cancel => self.finish(AirgapOutcome::Cancelled),
        }
        Task::none()
    }

    /// The exchange emits only its own actions; a caller maps them into whatever
    /// message its own flow speaks.
    pub fn view<M: Clone + 'static>(
        &self,
        to_message: impl Fn(AirgapAction) -> M + Clone + 'static,
    ) -> Element<'_, M> {
        let warning = self
            .error
            .as_ref()
            .map(ToString::to_string)
            .or_else(|| self.camera_error.as_ref().map(ToString::to_string))
            .map(|error| card::error("Air-gapped signer", error));

        let body = match self.phase {
            Phase::ShowRequest => self.request_view(to_message.clone(), false),
            Phase::StartingCamera => caption("Requesting camera access...").into(),
            Phase::Scanning => self.scanning_view(to_message.clone()),
            Phase::Done => Space::fill_width().into(),
        };

        let content = modal::modal_view(
            Some(self.title),
            None,
            Some(to_message(AirgapAction::Cancel)),
            modal::ModalWidth::XL,
            column![warning, body].spacing(20).width(Length::Fill),
        );
        if self.enlarged {
            // Closing the enlarged view returns to this same exchange.
            Modal::new(
                Container::new(content).center(Length::Fill),
                modal::modal_view(
                    Some(self.title),
                    None,
                    Some(to_message(AirgapAction::Enlarge(false))),
                    Length::Fill,
                    self.request_view(to_message.clone(), true),
                ),
            )
            .on_blur(Some(to_message(AirgapAction::Enlarge(false))))
            .into()
        } else {
            content
        }
    }

    fn request_view<M: Clone + 'static>(
        &self,
        to_message: impl Fn(AirgapAction) -> M + Clone + 'static,
        enlarged: bool,
    ) -> Element<'_, M> {
        let state = self.animation.as_ref().map(AnimatedQr::state);
        let frame = self
            .animation
            .as_ref()
            .and_then(AnimatedQr::frame)
            .map(|f| {
                qr::frame(
                    theme::qr_code::qr_code(&theme::Theme::default()),
                    self.brightness,
                    &f.data,
                    f.width,
                    f.height,
                    if enlarged {
                        Length::Fill
                    } else {
                        qr::FRAME_SIZE.into()
                    },
                    state.filter(|state| state.total_frames > 1).map(|state| {
                        format!("Frame {} of {}", state.frame + 1, state.total_frames)
                    }),
                )
            });

        let brightness = qr::brightness_slider(self.brightness, {
            let to_message = to_message.clone();
            move |brightness| to_message(AirgapAction::Brightness(brightness))
        });
        let scan = button::btn_scan_response(Some(to_message(AirgapAction::Scan)));
        if enlarged {
            return column![
                frame,
                row![brightness, Space::fill_width(), scan]
                    .align_y(iced::Alignment::Center)
                    .spacing(HSpacing::L),
            ]
            .spacing(VSpacing::M)
            .into();
        }

        column![
            b5_medium("Show this to your signer, then scan its answer back."),
            frame,
            brightness,
            row![
                button::btn_enlarge_qr(Some(to_message(AirgapAction::Enlarge(true)))),
                scan,
            ]
            .spacing(HSpacing::L),
        ]
        .spacing(VSpacing::M)
        .align_x(iced::Alignment::Center)
        .width(Length::Fill)
        .into()
    }

    fn scanning_view<M: Clone + 'static>(
        &self,
        to_message: impl Fn(AirgapAction) -> M + Clone + 'static,
    ) -> Element<'_, M> {
        let cameras = (self.cameras.len() > 1).then(|| {
            self.cameras
                .iter()
                .enumerate()
                .fold(row![].spacing(10), |chosen, (index, camera)| {
                    chosen.push(button::btn_secondary(
                        None,
                        &camera.name,
                        button::BtnWidth::Auto,
                        (self.selected_camera != Some(index))
                            .then(|| to_message(AirgapAction::SelectCamera(index))),
                    ))
                })
        });

        let status = match self.progress {
            Some(Progress { seen, total }) => format!("Read {seen} of {total} frames"),
            None => "Looking for the signer's QR code...".to_owned(),
        };

        column![
            b5_medium("Point the camera at your signer's screen."),
            cameras,
            self.preview
                .clone()
                .map(|handle| qr::preview(handle, PREVIEW_HEIGHT)),
            caption(status),
            button::btn_secondary(
                None,
                "Back to the request",
                button::BtnWidth::Auto,
                Some(to_message(AirgapAction::Back))
            ),
        ]
        .spacing(15)
        .align_x(iced::Alignment::Center)
        .width(Length::Fill)
        .into()
    }

    fn start_camera(&mut self, index: usize) {
        self.stop_camera();
        self.clear_errors();
        let Some(camera) = self.cameras.get(index) else {
            self.give_up_on_camera(CameraFailure::Unavailable);
            return;
        };
        match CameraScanner::start(camera.index.clone(), self.exchange.scan_config()) {
            Ok(scanner) => {
                self.selected_camera = Some(index);
                self.scanner = Some(scanner);
                self.phase = Phase::Scanning;
            }
            Err(failure) => self.give_up_on_camera(failure),
        }
    }

    fn poll_camera(&mut self) {
        let Some(scanner) = self.scanner.as_ref() else {
            return;
        };
        let mut payload = None;
        let mut failure = None;
        while let Ok(event) = scanner.try_recv() {
            match event {
                CameraEvent::Preview {
                    width,
                    height,
                    rgba,
                } => self.preview = Some(image::Handle::from_rgba(width, height, rgba)),
                CameraEvent::Progress(progress) => self.progress = Some(progress),
                CameraEvent::Payload(bytes) => payload = Some(bytes),
                CameraEvent::Failure(error) => failure = Some(error),
            }
        }
        if let Some(failure) = failure {
            self.give_up_on_camera(failure);
            return;
        }
        let Some(payload) = payload else {
            return;
        };
        match self.exchange.read(&payload) {
            Ok(answer) => {
                self.stop_camera();
                self.finish(AirgapOutcome::Answer(Box::new(answer)));
            }
            // The answer to an earlier exchange, still on the signer's screen.
            // That is the ordinary way a scan starts, not a failure, so say what
            // it is and keep looking rather than sending the user back a step.
            Err(error @ Error::RequestIdMismatch) => self.error = Some(error),
            Err(error) => {
                self.stop_camera();
                self.error = Some(error);
                self.phase = Phase::ShowRequest;
            }
        }
    }

    /// The camera cannot be used, so send the user back to the request they can
    /// still show, rather than leaving them on a dead scanning screen.
    fn give_up_on_camera(&mut self, failure: CameraFailure) {
        self.stop_camera();
        self.camera_error = Some(failure);
        self.phase = Phase::ShowRequest;
    }

    fn stop_camera(&mut self) {
        self.scanner = None;
        self.preview = None;
        self.progress = None;
        self.selected_camera = None;
    }

    fn clear_errors(&mut self) {
        self.error = None;
        self.camera_error = None;
    }

    fn finish(&mut self, outcome: AirgapOutcome) {
        self.stop_camera();
        self.enlarged = false;
        self.animation = None;
        self.phase = Phase::Done;
        self.outcome = Some(outcome);
    }
}

/// The signer echoes this back so Liana can tell its own answer from a stale one
/// still on screen from an earlier exchange.
fn request_id() -> bwk_qr::protocol::RequestId {
    let mut id = [0u8; bwk_qr::protocol::REQUEST_ID_LEN];
    getrandom::fill(&mut id).expect("the OS random source is required");
    bwk_qr::protocol::RequestId(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::{
        advanced::{layout, renderer::Headless, widget::Tree, Layout, Shell},
        mouse, Event, Size, Vector,
    };

    fn modal() -> AirgapModal {
        AirgapModal::new(
            "Register wallet",
            Ask::Register {
                wallet: crate::airgap::Wallet {
                    alias: "Liana".to_owned(),
                    descriptor: include_str!(
                        "../../../test_assets/airgap/multisig-testnet.descriptor"
                    )
                    .trim()
                    .parse()
                    .unwrap(),
                    registration: Default::default(),
                },
            },
        )
        .unwrap()
    }

    #[test]
    fn enlarging_preserves_the_exchange_and_brightness() {
        let mut modal = modal();
        let frames = modal.exchange.frames().to_vec();
        let time = std::time::Instant::now() + Duration::from_secs(1);
        let state = modal.animation.as_ref().unwrap().state_at(time);
        assert!(state.total_frames > 1);
        let _ = modal.update(AirgapAction::Brightness(Brightness::new(70)));
        for enlarged in [true, false, true, false] {
            let _ = modal.update(AirgapAction::Enlarge(enlarged));
            assert_eq!(modal.enlarged, enlarged);
            assert_eq!(modal.phase, Phase::ShowRequest);
            assert_eq!(modal.exchange.frames(), frames);
            assert_eq!(modal.animation.as_ref().unwrap().state_at(time), state);
            assert_eq!(modal.brightness.percent(), 70);
            assert!(modal.take_outcome().is_none());
        }
    }

    #[test]
    fn starting_capture_leaves_the_enlarged_view() {
        let mut modal = modal();
        let _ = modal.update(AirgapAction::Enlarge(true));
        let _ = modal.update(AirgapAction::Scan);
        assert!(!modal.enlarged);
        assert_eq!(modal.phase, Phase::StartingCamera);
        let _ = modal.update(AirgapAction::Enlarge(true));
        assert!(!modal.enlarged);
        let _ = modal.update(AirgapAction::Back);
        assert_eq!(modal.phase, Phase::ShowRequest);
        assert!(!modal.enlarged);
    }

    #[tokio::test]
    async fn enlarged_view_fits_and_its_close_button_returns_to_the_request() {
        let renderer = iced::Renderer::new(iced::Font::DEFAULT, 16.into(), Some("tiny-skia"))
            .await
            .unwrap();
        for size in [
            Size::new(1000.0, 650.0),
            Size::new(1280.0, 900.0),
            Size::new(1920.0, 1080.0),
        ] {
            let mut modal = modal();
            let _ = modal.update(AirgapAction::Enlarge(true));
            let mut messages = Vec::new();
            {
                let mut view = modal.view(|action| action);
                let mut tree = Tree::new(&view);
                let node = view.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, size),
                );
                let mut overlay = view
                    .as_widget_mut()
                    .overlay(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        &iced::Rectangle::with_size(size),
                        Vector::ZERO,
                    )
                    .unwrap();
                let node = overlay.as_overlay_mut().layout(&renderer, size);
                let column = Layout::new(&node)
                    .children()
                    .next()
                    .unwrap()
                    .children()
                    .next()
                    .unwrap();
                let header = column.children().next().unwrap();
                let body = column.children().nth(1).unwrap();
                let qr = body
                    .children()
                    .next()
                    .unwrap()
                    .children()
                    .next()
                    .unwrap()
                    .children()
                    .next()
                    .unwrap()
                    .bounds();
                let controls = body.children().last().unwrap().bounds();
                assert!(qr.width > 0.0 && qr.height > 0.0);
                assert!(qr.y + qr.height <= controls.y);
                assert!(controls.y + controls.height <= size.height);
                if size.height >= 900.0 {
                    assert!(qr.width.min(qr.height) > qr::FRAME_SIZE as f32);
                }
                let close = header.children().last().unwrap().bounds().center();
                for event in [
                    mouse::Event::ButtonPressed(mouse::Button::Left),
                    mouse::Event::ButtonReleased(mouse::Button::Left),
                ] {
                    overlay.as_overlay_mut().update(
                        &Event::Mouse(event),
                        Layout::new(&node),
                        mouse::Cursor::Available(close),
                        &renderer,
                        &mut iced::advanced::clipboard::Null,
                        &mut Shell::new(&mut messages),
                    );
                }
            }
            assert!(matches!(
                messages.as_slice(),
                [AirgapAction::Enlarge(false)]
            ));
            let _ = modal.update(messages.pop().unwrap());
            assert!(!modal.enlarged);
            assert_eq!(modal.phase, Phase::ShowRequest);
            assert!(modal.take_outcome().is_none());
        }
    }
}
