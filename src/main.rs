use std::{collections::HashMap, net::SocketAddr};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    serve, Json, Router,
};
use color_eyre::eyre::{Report, Result};
use futures::TryStreamExt as _;
use neo4rs::{ConfigBuilder, Graph};
use serde::{Deserialize, Serialize};
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::{debug, instrument};
use tracing_error::ErrorLayer;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .with(ErrorLayer::default())
        .init();

    let db = db().await?;
    let service = Service { db };

    let assets_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");

    let app = Router::new()
        .route("/", get(|| async { Redirect::temporary("/index.html") }))
        .route("/movie/:title", get(movie))
        .route("/movie/vote/:title", post(vote))
        .route("/search", get(search))
        .route("/graph", get(graph))
        .fallback_service(ServeDir::new(assets_dir))
        .layer(TraceLayer::new_for_http())
        .with_state(service);

    let port = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    debug!("listening on {}", listener.local_addr().unwrap());

    serve(listener, app).await?;

    Ok(())
}

async fn db() -> Result<Graph> {
    const DEFAULT_URL: &str = "neo4j+s://demo.neo4jlabs.com";
    const DEFAULT_DATABASE: &str = "movies";
    const DEFAULT_USER: &str = "movies";
    const DEFAULT_PASS: &str = "movies";

    let config = ConfigBuilder::new()
        .uri(
            std::env::var("NEO4J_URI")
                .ok()
                .filter(|s| !s.is_empty())
                .as_deref()
                .unwrap_or(DEFAULT_URL),
        )
        .user(
            std::env::var("NEO4J_USER")
                .ok()
                .filter(|s| !s.is_empty())
                .as_deref()
                .unwrap_or(DEFAULT_USER),
        )
        .password(
            std::env::var("NEO4J_PASSWORD")
                .ok()
                .filter(|s| !s.is_empty())
                .as_deref()
                .unwrap_or(DEFAULT_PASS),
        )
        .db(std::env::var("NEO4J_DATABASE")
            .ok()
            .filter(|s| !s.is_empty())
            .as_deref()
            .unwrap_or(DEFAULT_DATABASE))
        .build()?;

    Ok(Graph::connect(config).await?)
}

async fn movie(
    Path(title): Path<String>,
    State(service): State<Service>,
) -> Result<Json<Movie>, AppError> {
    Ok(Json(service.movie(title).await?))
}

async fn vote(
    Path(title): Path<String>,
    State(service): State<Service>,
) -> Result<Json<Voted>, AppError> {
    Ok(Json(service.vote(title).await?))
}

async fn search(
    Query(search): Query<Search>,
    State(service): State<Service>,
) -> Result<Json<Vec<MovieResult>>, AppError> {
    Ok(Json(service.search(search).await?))
}

async fn graph(
    Query(browse): Query<Browse>,
    State(service): State<Service>,
) -> Result<Json<BrowseResponse>, AppError> {
    Ok(Json(service.graph(browse).await?))
}

#[derive(Clone)]
struct Service {
    db: Graph,
}

impl Service {
    #[instrument(skip(self))]
    async fn movie(&self, title: String) -> Result<Movie> {
        const FIND_MOVIE: &str = "
            MATCH (movie:Movie {title:$title})
            OPTIONAL MATCH (movie)<-[r]-(person:Person)
            WITH movie.title AS title,
            collect({
                name:person.name,
                job: head(split(toLower(type(r)),'_')),
                role: r.roles
            }) AS cast
            LIMIT 1
            RETURN title, cast";

        let mut rows = self
            .db
            .execute(neo4rs::query(FIND_MOVIE).param("title", title))
            .await?;

        // TODO: next_as::<Movie>()?
        let movie = rows
            .next()
            .await?
            .map(|r| r.to::<Movie>())
            .transpose()?
            .unwrap_or_default();

        // TODO: make this possible
        // TODO: let summary = rows.finish().await?;
        // TODO: debug!(?summary);

        debug!(?movie);

        Ok(movie)
    }

    #[instrument(skip(self))]
    async fn vote(&self, title: String) -> Result<Voted> {
        const VOTE_IN_MOVIE: &str = "
            MATCH (movie:Movie {title:$title})
            SET movie.votes = coalesce(movie.votes, 0) + 1
            RETURN movie.votes";

        self.db
            .run(neo4rs::query(VOTE_IN_MOVIE).param("title", title))
            .await?;

        // TODO:
        // let summary = self.db.run(...).await?;

        Ok(Voted { updates: 1 })
    }

    #[instrument(skip(self))]
    async fn search(&self, search: Search) -> Result<Vec<MovieResult>> {
        const SEARCH_MOVIES: &str = "
          MATCH (movie:Movie)
          WHERE toLower(movie.title) CONTAINS toLower($part)
          RETURN movie";

        let rows = self
            .db
            .execute(neo4rs::query(SEARCH_MOVIES).param("part", search.q))
            .await?;

        let movies = rows.into_stream_as::<MovieResult>().try_collect().await?;

        debug!(?movies);

        Ok(movies)
    }

    #[instrument(skip(self))]
    async fn graph(&self, browse: Browse) -> Result<BrowseResponse> {
        const GRAPH: &str = "
            MATCH (m:Movie)<-[:ACTED_IN]-(a:Person)
            RETURN m.title as movie, collect(a.name) as cast
            LIMIT $limit";

        let limit = browse.limit.unwrap_or(100);

        let mut rows = self
            .db
            .execute(neo4rs::query(GRAPH).param("limit", limit))
            .await?;

        let mut actors = HashMap::<String, usize>::new();

        let mut nodes = Vec::new();
        let mut links = Vec::new();

        while let Some(row) = rows.next().await? {
            let movie = row.get::<String>("movie")?;
            let target = nodes.len();

            nodes.push(Node {
                title: movie,
                label: "movie",
            });

            let cast = row.get::<Vec<&str>>("cast")?;
            for actor in cast {
                let source = match actors.get(actor) {
                    Some(&source) => source,
                    None => {
                        let source = nodes.len();
                        actors.insert(actor.to_owned(), source);

                        nodes.push(Node {
                            title: actor.to_owned(),
                            label: "actor",
                        });
                        source
                    }
                };
                links.push(Link { source, target });
            }
        }

        let response = BrowseResponse { nodes, links };
        Ok(response)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Search {
    q: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Browse {
    limit: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Movie {
    released: Option<u32>,
    title: Option<String>,
    tagline: Option<String>,
    votes: Option<usize>,
    cast: Option<Vec<Person>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MovieResult {
    movie: Movie,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Person {
    job: String,
    role: Option<Vec<String>>,
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Voted {
    updates: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(deserialize = "'de: 'static"))]
struct BrowseResponse {
    nodes: Vec<Node>,
    links: Vec<Link>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Node {
    title: String,
    label: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Link {
    source: usize,
    target: usize,
}

struct AppError(Report);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.0),
        )
            .into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<Report>,
{
    fn from(err: E) -> Self {
        let err = err.into();
        debug!("error: {:?}", err);
        Self(err)
    }
}
