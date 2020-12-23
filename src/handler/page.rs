use std::sync::Arc;

use futures::channel::mpsc::{channel, Receiver, Sender};
use futures::channel::oneshot::channel as oneshot_channel;
use futures::stream::Fuse;
use futures::{SinkExt, StreamExt};

use chromiumoxide_cdp::cdp::browser_protocol::browser::{GetVersionParams, GetVersionReturns};
use chromiumoxide_cdp::cdp::browser_protocol::dom::{
    NodeId, QuerySelectorAllParams, QuerySelectorParams,
};
use chromiumoxide_cdp::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    MouseButton,
};
use chromiumoxide_cdp::cdp::browser_protocol::page::{
    CaptureScreenshotParams, GetLayoutMetricsParams, GetLayoutMetricsReturns,
};
use chromiumoxide_cdp::cdp::browser_protocol::target::{ActivateTargetParams, SessionId, TargetId};
use chromiumoxide_cdp::cdp::js_protocol::runtime::{
    CallFunctionOnParams, CallFunctionOnReturns, EvaluateParams, ExecutionContextId, RemoteObjectId,
};
use chromiumoxide_types::{Command, CommandResponse};

use crate::cmd::{to_command_response, CommandMessage};
use crate::error::{CdpError, Result};
use crate::handler::domworld::DOMWorldKind;
use crate::handler::target::{GetExecutionContext, TargetMessage};
use crate::js::EvaluationResult;
use crate::keys;
use crate::layout::Point;

#[derive(Debug)]
pub struct PageHandle {
    pub(crate) rx: Fuse<Receiver<TargetMessage>>,
    page: Arc<PageInner>,
}

impl PageHandle {
    pub fn new(target_id: TargetId, session_id: SessionId) -> Self {
        let (commands, rx) = channel(1);
        let page = PageInner {
            target_id,
            session_id,
            sender: commands,
        };
        Self {
            rx: rx.fuse(),
            page: Arc::new(page),
        }
    }

    pub(crate) fn inner(&self) -> &Arc<PageInner> {
        &self.page
    }
}

#[derive(Debug)]
pub(crate) struct PageInner {
    target_id: TargetId,
    session_id: SessionId,
    sender: Sender<TargetMessage>,
}

impl PageInner {
    /// Execute a PDL command and return its response
    pub(crate) async fn execute<T: Command>(&self, cmd: T) -> Result<CommandResponse<T::Response>> {
        Ok(execute(cmd, self.sender.clone(), Some(self.session_id.clone())).await?)
    }

    /// This responds with the current url of the page, once the navigation
    /// finished and the page is loaded
    pub(crate) async fn wait_for_navigation(&self) -> Result<String> {
        let (tx, rx) = oneshot_channel();
        self.sender
            .clone()
            .send(TargetMessage::WaitForNavigation(tx))
            .await?;
        Ok(rx.await??)
    }

    /// The identifier of this page's target
    pub fn target_id(&self) -> &TargetId {
        &self.target_id
    }

    /// The identifier of this page's target's session
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub(crate) fn sender(&self) -> &Sender<TargetMessage> {
        &self.sender
    }

    /// Returns the first element in the node which matches the given CSS
    /// selector.
    pub async fn find_element(&self, selector: impl Into<String>, node: NodeId) -> Result<NodeId> {
        Ok(self
            .execute(QuerySelectorParams::new(node, selector))
            .await?
            .node_id)
    }

    /// Activates (focuses) the target.
    pub async fn activate(&self) -> Result<&Self> {
        self.execute(ActivateTargetParams::new(self.target_id().clone()))
            .await?;
        Ok(self)
    }

    /// Version information about the browser
    pub async fn version(&self) -> Result<GetVersionReturns> {
        Ok(self.execute(GetVersionParams::default()).await?.result)
    }

    /// Return all `Element`s inside the node that match the given selector
    pub(crate) async fn find_elements(
        &self,
        selector: impl Into<String>,
        node: NodeId,
    ) -> Result<Vec<NodeId>> {
        Ok(self
            .execute(QuerySelectorAllParams::new(node, selector))
            .await?
            .result
            .node_ids)
    }

    /// Moves the mouse to this point (dispatches a mouseMoved event)
    pub async fn move_mouse(&self, point: Point) -> Result<&Self> {
        self.execute(DispatchMouseEventParams::new(
            DispatchMouseEventType::MouseMoved,
            point.x,
            point.y,
        ))
        .await?;
        Ok(self)
    }

    /// Performs a mouse click event at the point's location
    pub async fn click(&self, point: Point) -> Result<&Self> {
        let cmd = DispatchMouseEventParams::builder()
            .x(point.x)
            .y(point.y)
            .button(MouseButton::Left)
            .click_count(1);

        self.move_mouse(point)
            .await?
            .execute(
                cmd.clone()
                    .r#type(DispatchMouseEventType::MousePressed)
                    .build()
                    .unwrap(),
            )
            .await?;

        self.execute(
            cmd.r#type(DispatchMouseEventType::MouseReleased)
                .build()
                .unwrap(),
        )
        .await?;
        Ok(self)
    }

    /// This simulates pressing keys on the page.
    ///
    /// # Note The `input` is treated as series of `KeyDefinition`s, where each
    /// char is inserted as a separate keystroke. So sending
    /// `page.type_str("Enter")` will be processed as a series of single
    /// keystrokes:  `["E", "n", "t", "e", "r"]`. To simulate pressing the
    /// actual Enter key instead use `page.press_key(
    /// keys::get_key_definition("Enter").unwrap())`.
    pub async fn type_str(&self, input: impl AsRef<str>) -> Result<&Self> {
        for c in input.as_ref().split("").filter(|s| !s.is_empty()) {
            self.press_key(c).await?;
        }
        Ok(self)
    }

    /// Uses the `DispatchKeyEvent` mechanism to simulate pressing keyboard
    /// keys.
    pub async fn press_key(&self, key: impl AsRef<str>) -> Result<&Self> {
        let key = key.as_ref();
        let key_definition = keys::get_key_definition(key)
            .ok_or_else(|| CdpError::msg(format!("Key not found: {}", key)))?;
        let mut cmd = DispatchKeyEventParams::builder();

        // See https://github.com/GoogleChrome/puppeteer/blob/62da2366c65b335751896afbb0206f23c61436f1/lib/Input.js#L114-L115
        // And https://github.com/GoogleChrome/puppeteer/blob/62da2366c65b335751896afbb0206f23c61436f1/lib/Input.js#L52
        let key_down_event_type = if let Some(txt) = key_definition.text {
            cmd = cmd.text(txt);
            DispatchKeyEventType::KeyDown
        } else if key_definition.key.len() == 1 {
            cmd = cmd.text(key_definition.key);
            DispatchKeyEventType::KeyDown
        } else {
            DispatchKeyEventType::RawKeyDown
        };

        cmd = cmd
            .r#type(DispatchKeyEventType::KeyDown)
            .key(key_definition.key)
            .code(key_definition.code)
            .windows_virtual_key_code(key_definition.key_code)
            .native_virtual_key_code(key_definition.key_code);

        self.execute(cmd.clone().r#type(key_down_event_type).build().unwrap())
            .await?;
        self.execute(cmd.r#type(DispatchKeyEventType::KeyUp).build().unwrap())
            .await?;
        Ok(self)
    }

    /// Calls function with given declaration on the remote object with the
    /// matching id
    pub async fn call_js_fn(
        &self,
        function_declaration: impl Into<String>,
        await_promise: bool,
        remote_object_id: RemoteObjectId,
    ) -> Result<CallFunctionOnReturns> {
        let resp = self
            .execute(
                CallFunctionOnParams::builder()
                    .object_id(remote_object_id)
                    .function_declaration(function_declaration)
                    .generate_preview(true)
                    .await_promise(await_promise)
                    .build()
                    .unwrap(),
            )
            .await?;
        Ok(resp.result)
    }

    pub async fn evaluate_expression(
        &self,
        evaluate: impl Into<EvaluateParams>,
    ) -> Result<EvaluationResult> {
        let mut evaluate = evaluate.into();
        if evaluate.context_id.is_none() {
            evaluate.context_id = self.execution_context().await?;
        }
        if evaluate.await_promise.is_none() {
            evaluate.await_promise = Some(true);
        }
        if evaluate.return_by_value.is_none() {
            evaluate.return_by_value = Some(true);
        }

        let resp = self.execute(evaluate).await?.result;
        if let Some(exception) = resp.exception_details {
            return Err(CdpError::JavascriptException(Box::new(exception)));
        }

        Ok(EvaluationResult::new(resp.result))
    }

    pub async fn evaluate_function(
        &self,
        evaluate: impl Into<CallFunctionOnParams>,
    ) -> Result<EvaluationResult> {
        let mut evaluate = evaluate.into();
        if evaluate.execution_context_id.is_none() {
            evaluate.execution_context_id = self.execution_context().await?;
        }
        if evaluate.await_promise.is_none() {
            evaluate.await_promise = Some(true);
        }
        if evaluate.return_by_value.is_none() {
            evaluate.return_by_value = Some(true);
        }

        let resp = self.execute(evaluate).await?.result;
        if let Some(exception) = resp.exception_details {
            return Err(CdpError::JavascriptException(Box::new(exception)));
        }
        Ok(EvaluationResult::new(resp.result))
    }

    pub async fn execution_context(&self) -> Result<Option<ExecutionContextId>> {
        Ok(self.execution_context_for_world(DOMWorldKind::Main).await?)
    }

    pub async fn secondary_execution_context(&self) -> Result<Option<ExecutionContextId>> {
        Ok(self
            .execution_context_for_world(DOMWorldKind::Secondary)
            .await?)
    }

    pub async fn execution_context_for_world(
        &self,
        dom_world: DOMWorldKind,
    ) -> Result<Option<ExecutionContextId>> {
        let (tx, rx) = oneshot_channel();
        self.sender
            .clone()
            .send(TargetMessage::GetExecutionContext(GetExecutionContext {
                dom_world,
                frame_id: None,
                tx,
            }))
            .await?;
        Ok(rx.await?)
    }

    /// Returns metrics relating to the layout of the page
    pub async fn layout_metrics(&self) -> Result<GetLayoutMetricsReturns> {
        Ok(self
            .execute(GetLayoutMetricsParams::default())
            .await?
            .result)
    }

    pub async fn screenshot(&self, params: impl Into<CaptureScreenshotParams>) -> Result<Vec<u8>> {
        self.activate().await?;
        let params = params.into();
        let res = self.execute(params).await?.result;
        Ok(base64::decode(&res.data)?)
    }
}

pub(crate) async fn execute<T: Command>(
    cmd: T,
    mut sender: Sender<TargetMessage>,
    session: Option<SessionId>,
) -> Result<CommandResponse<T::Response>> {
    let (tx, rx) = oneshot_channel();
    let method = cmd.identifier();
    let msg = CommandMessage::with_session(cmd, tx, session)?;

    sender.send(TargetMessage::Command(msg)).await?;
    let resp = rx.await??;
    to_command_response::<T>(resp, method)
}
