use anyhow::Context;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use axum::Router;
use parking_lot::lock_api::MappedMutexGuard;
use parking_lot::{Mutex, MutexGuard, RawMutex};
use serde::Serialize;
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tower::Service;

use crate::github::api::load_repositories;
use crate::tests::database::MockedDBClient;
use crate::tests::event::default_pr_number;
use crate::tests::mocks::comment::{Comment, GitHubIssueCommentEventPayload};
use crate::tests::mocks::workflow::{
    CheckSuite, GitHubCheckRunEventPayload, GitHubCheckSuiteEventPayload,
    GitHubWorkflowEventPayload, TestWorkflowStatus, Workflow, WorkflowEvent, WorkflowEventKind,
};
use crate::tests::mocks::{default_repo_name, Branch, ExternalHttpMock, Repo, World};
use crate::tests::webhook::{create_webhook_request, TEST_WEBHOOK_SECRET};
use crate::{
    create_app, create_bors_process, BorsContext, CommandParser, ServerState, WebhookSecret,
};

pub struct BorsBuilder {
    world: World,
    pool: PgPool,
}

impl BorsBuilder {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            world: Default::default(),
        }
    }

    pub fn world(self, world: World) -> Self {
        Self { world, ..self }
    }

    /// This closure is used to ensure that the test has to return `BorsTester`
    /// to us, so that we can call `finish()` on it. Without that call, we couldn't
    /// ensure that some async task within the bors process hasn't crashed.
    pub async fn run_test<
        F: FnOnce(BorsTester) -> Fut,
        Fut: Future<Output = anyhow::Result<BorsTester>>,
    >(
        self,
        f: F,
    ) -> World {
        // We return `tester` and `bors` separately, so that we can finish `bors`
        // even if `f` returns a result, for better error propagation.
        let (tester, bors) = BorsTester::new(self.pool, self.world).await;
        match f(tester).await {
            Ok(tester) => tester.finish(bors).await,
            Err(error) => {
                let result = bors.await;
                panic!("Error in run_test\n{result:?}\n{error:?}");
            }
        }
    }
}

/// Simple end-to-end test entrypoint for tests that don't need to prepare any custom state.
pub async fn run_test<
    F: FnOnce(BorsTester) -> Fut,
    Fut: Future<Output = anyhow::Result<BorsTester>>,
>(
    pool: PgPool,
    f: F,
) -> World {
    BorsBuilder::new(pool).run_test(f).await
}

/// Represents a running bors web application. This structure should be used
/// in tests to test interaction with the bot.
///
/// Dropping this struct will drop `app`, which will close the
/// send channels for the bors process, which should stop its async task.
pub struct BorsTester {
    app: Router,
    http_mock: ExternalHttpMock,
    world: World,
    db: Arc<MockedDBClient>,
}

impl BorsTester {
    async fn new(pool: PgPool, world: World) -> (Self, JoinHandle<()>) {
        let mock = ExternalHttpMock::start(&world).await;
        let db = Arc::new(MockedDBClient::new(pool));

        let loaded_repos = load_repositories(&mock.github_client(), &mock.team_api_client())
            .await
            .unwrap();
        let mut repos = HashMap::default();
        for (name, repo) in loaded_repos {
            let repo = repo.unwrap();
            repos.insert(name, Arc::new(repo));
        }

        let ctx = BorsContext::new(CommandParser::new("@bors".to_string()), db.clone(), repos);

        let (repository_tx, global_tx, bors_process) =
            create_bors_process(ctx, mock.github_client(), mock.team_api_client());

        let state = ServerState::new(
            repository_tx,
            global_tx,
            WebhookSecret::new(TEST_WEBHOOK_SECRET.to_string()),
        );
        let app = create_app(state);
        let bors = tokio::spawn(bors_process);
        (
            Self {
                app,
                http_mock: mock,
                world,
                db,
            },
            bors,
        )
    }

    pub fn db(&self) -> Arc<MockedDBClient> {
        self.db.clone()
    }

    pub fn default_repo(&mut self) -> Arc<Mutex<Repo>> {
        self.world.get_repo(default_repo_name())
    }

    pub fn create_branch(&mut self, name: &str) -> MappedMutexGuard<RawMutex, Branch> {
        // We cannot clone the Arc, otherwise it won't work
        let repo = self.world.repos.get(&default_repo_name()).unwrap();
        let mut repo = repo.lock();

        // Polonius where art thou :/
        if repo.get_branch_by_name(name).is_some() {
            MutexGuard::map(repo, |repo| repo.get_branch_by_name(name).unwrap())
        } else {
            MutexGuard::map(repo, move |repo| {
                repo.branches
                    .push(Branch::new(name, &format!("{name}-empty")));
                repo.branches.last_mut().unwrap()
            })
        }
    }

    pub fn get_branch_mut(&mut self, name: &str) -> MappedMutexGuard<RawMutex, Branch> {
        let repo = self.world.repos.get(&default_repo_name()).unwrap();
        MutexGuard::map(repo.lock(), move |repo| {
            repo.get_branch_by_name(name).unwrap()
        })
    }

    pub fn get_branch(&self, name: &str) -> Branch {
        self.world
            .default_repo()
            .lock()
            .get_branch_by_name(name)
            .unwrap()
            .clone()
    }

    pub fn try_branch(&self) -> Branch {
        self.get_branch("automation/bors/try")
    }

    /// Wait until the next bot comment is received on the default repo and the default PR.
    pub async fn get_comment(&mut self) -> anyhow::Result<String> {
        Ok(self
            .http_mock
            .gh_server
            .get_comment(Repo::default().name, default_pr_number())
            .await?
            .content)
    }

    /// Expect that `count` comments will be received, without checking their contents.
    pub async fn expect_comments(&mut self, count: u64) {
        for _ in 0..count {
            let _ = self.get_comment().await;
        }
    }

    pub async fn post_comment<C: Into<Comment>>(&mut self, comment: C) -> anyhow::Result<()> {
        self.webhook_comment(comment.into()).await
    }

    /// Performs a single started/success/failure workflow event.
    pub async fn workflow_event(&mut self, event: WorkflowEvent) -> anyhow::Result<()> {
        if let Some(branch) = self
            .world
            .get_repo(event.workflow.repository.clone())
            .lock()
            .get_branch_by_name(&event.workflow.head_branch)
        {
            match &event.event {
                WorkflowEventKind::Started => {}
                WorkflowEventKind::Completed { status } => {
                    let status = match status.as_str() {
                        "success" => TestWorkflowStatus::Success,
                        "failure" => TestWorkflowStatus::Failure,
                        _ => unreachable!(),
                    };
                    branch.suite_finished(status);
                }
            }
        }
        self.webhook_workflow(event).await
    }

    /// Performs all necessary events to complete a single workflow (start, success/fail,
    /// check suite completed).
    #[inline]
    pub async fn workflow_full<W: Into<Workflow>>(
        &mut self,
        workflow: W,
        status: TestWorkflowStatus,
    ) -> anyhow::Result<()> {
        let workflow = workflow.into();
        let branch = self.get_branch(&workflow.head_branch);

        if !workflow.external {
            self.workflow_event(WorkflowEvent::started(workflow.clone()))
                .await?;
            let event = match status {
                TestWorkflowStatus::Success => WorkflowEvent::success(workflow.clone()),
                TestWorkflowStatus::Failure => WorkflowEvent::failure(workflow.clone()),
            };
            self.workflow_event(event).await?;
        } else {
            self.webhook_external_workflow(workflow).await?;
        }

        self.check_suite(CheckSuite::completed(branch)).await
    }

    pub async fn workflow_success<W: Into<Workflow>>(&mut self, workflow: W) -> anyhow::Result<()> {
        self.workflow_full(workflow, TestWorkflowStatus::Success)
            .await
    }

    pub async fn workflow_failure<W: Into<Workflow>>(&mut self, workflow: W) -> anyhow::Result<()> {
        self.workflow_full(workflow, TestWorkflowStatus::Failure)
            .await
    }

    pub async fn check_suite<C: Into<CheckSuite>>(&mut self, check_suite: C) -> anyhow::Result<()> {
        self.webhook_check_suite(check_suite.into()).await
    }

    async fn webhook_comment(&mut self, comment: Comment) -> anyhow::Result<()> {
        self.send_webhook(
            "issue_comment",
            // The Box is here to prevent a stack overflow in debug mode
            Box::from(GitHubIssueCommentEventPayload::from(comment)),
        )
        .await
    }

    async fn webhook_workflow(&mut self, event: WorkflowEvent) -> anyhow::Result<()> {
        self.send_webhook("workflow_run", GitHubWorkflowEventPayload::from(event))
            .await
    }

    async fn webhook_external_workflow(&mut self, workflow: Workflow) -> anyhow::Result<()> {
        self.send_webhook("check_run", GitHubCheckRunEventPayload::from(workflow))
            .await
    }

    async fn webhook_check_suite(&mut self, check_suite: CheckSuite) -> anyhow::Result<()> {
        self.send_webhook(
            "check_suite",
            GitHubCheckSuiteEventPayload::from(check_suite),
        )
        .await
    }

    async fn send_webhook<S: Serialize>(&mut self, event: &str, content: S) -> anyhow::Result<()> {
        let serialized = serde_json::to_string(&content).unwrap();
        let webhook = create_webhook_request(event, &serialized);
        let response = self
            .app
            .call(webhook)
            .await
            .context("Cannot send webhook request")?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "Wrong status code {status} when sending {event}"
            ));
        }
        let body_text = String::from_utf8(
            axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
                .await?
                .to_vec(),
        )?;
        tracing::debug!("Received webhook with status {status} and response body `{body_text}`");
        Ok(())
    }

    pub async fn finish(self, bors: JoinHandle<()>) -> World {
        // Make sure that the event channel senders are closed
        drop(self.app);
        // Wait until all events are handled in the bors service
        bors.await.unwrap();
        // Flush any local queues
        self.http_mock.gh_server.assert_empty_queues().await;
        self.world
    }
}
