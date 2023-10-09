use std::collections::hash_map::Entry;
use std::{collections::HashMap, sync::Arc};

use egui::{pos2, Color32, Context, Mesh, Rect, Vec2};
use egui_extras::RetainedImage;
use futures::StreamExt;
use reqwest::header::USER_AGENT;

use crate::io::Runtime;
use crate::mercator::TileId;
use crate::providers::TileSource;

#[derive(Clone)]
pub struct Tile {
    image: Arc<RetainedImage>,
}

impl Tile {
    fn from_image_bytes(image: &[u8]) -> Result<Self, String> {
        RetainedImage::from_image_bytes("debug_name", image).map(|image| Self {
            image: Arc::new(image),
        })
    }

    pub fn rect(&self, screen_position: Vec2) -> Rect {
        let tile_size = pos2(self.image.width() as f32, self.image.height() as f32);
        Rect::from_two_pos(
            screen_position.to_pos2(),
            (screen_position + tile_size.to_vec2()).to_pos2(),
        )
    }

    pub fn mesh(&self, screen_position: Vec2, ctx: &Context) -> Mesh {
        let mut mesh = Mesh::with_texture(self.image.texture_id(ctx));
        mesh.add_rect_with_uv(
            self.rect(screen_position),
            Rect::from_min_max(pos2(0., 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        mesh
    }
}

/// Downloads and keeps cache of the tiles. It must persist between frames.
pub struct Tiles {
    cache: HashMap<TileId, Option<Tile>>,

    /// Tiles to be downloaded by the IO thread.
    request_tx: futures::channel::mpsc::Sender<TileId>,

    /// Tiles that got downloaded and should be put in the cache.
    tile_rx: futures::channel::mpsc::Receiver<(TileId, Tile)>,

    #[allow(dead_code)] // Significant Drop
    runtime: Runtime,
}

impl Tiles {
    pub fn new<S>(source: S, egui_ctx: Context) -> Self
    where
        S: TileSource + Send + 'static,
    {
        // Minimum value which didn't cause any stalls while testing.
        let channel_size = 20;

        let (request_tx, request_rx) = futures::channel::mpsc::channel(channel_size);
        let (tile_tx, tile_rx) = futures::channel::mpsc::channel(channel_size);
        let runtime = Runtime::new(download_wrap(source, request_rx, tile_tx, egui_ctx));

        Self {
            cache: Default::default(),
            request_tx,
            tile_rx,
            runtime,
        }
    }

    /// Return a tile if already in cache, schedule a download otherwise.
    pub fn at(&mut self, tile_id: TileId) -> Option<Tile> {
        // Just take one at the time.
        match self.tile_rx.try_next() {
            Ok(Some((tile_id, tile))) => {
                self.cache.insert(tile_id, Some(tile));
            }
            Err(_) => {
                // Just ignore. It means that no new tile was downloaded.
            }
            Ok(None) => panic!("IO thread is dead"),
        }

        match self.cache.entry(tile_id) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                if let Ok(()) = self.request_tx.try_send(tile_id) {
                    log::debug!("Requested tile: {:?}", tile_id);
                    entry.insert(None);
                } else {
                    log::debug!("Request queue is full.");
                }
                None
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Http(reqwest::Error),

    #[error("error while decoding the image: {0}")]
    Image(String),
}

async fn download_single(client: &reqwest::Client, url: &str) -> Result<Tile, Error> {
    let image = client
        .get(url)
        .header(USER_AGENT, "Walkers")
        .send()
        .await
        .map_err(Error::Http)?;

    log::debug!("Downloaded {:?}.", image.status());

    let image = image
        .error_for_status()
        .map_err(Error::Http)?
        .bytes()
        .await
        .map_err(Error::Http)?;

    Tile::from_image_bytes(&image).map_err(Error::Image)
}

async fn download<S>(
    source: S,
    mut request_rx: futures::channel::mpsc::Receiver<TileId>,
    mut tile_tx: futures::channel::mpsc::Sender<(TileId, Tile)>,
    egui_ctx: Context,
) -> Result<(), ()>
where
    S: TileSource + Send + 'static,
{
    // Keep outside the loop to reuse it as much as possible.
    let client = reqwest::Client::new();

    loop {
        let request = request_rx.next().await.ok_or(())?;
        let url = source.tile_url(request);

        log::debug!("Getting {:?} from {}.", request, url);

        match download_single(&client, &url).await {
            Ok(tile) => {
                tile_tx.try_send((request, tile)).map_err(|_| ())?;
                egui_ctx.request_repaint();
            }
            Err(e) => {
                log::warn!("Could not download '{}': {}", &url, e);
            }
        }
    }
}

async fn download_wrap<S>(
    source: S,
    request_rx: futures::channel::mpsc::Receiver<TileId>,
    tile_tx: futures::channel::mpsc::Sender<(TileId, Tile)>,
    egui_ctx: Context,
) where
    S: TileSource + Send + 'static,
{
    if download(source, request_rx, tile_tx, egui_ctx)
        .await
        .is_err()
    {
        log::error!("Error from IO runtime.");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    static TILE_ID: TileId = TileId {
        x: 1,
        y: 2,
        zoom: 3,
    };

    type Source = Box<dyn Fn(TileId) -> String + Send>;

    /// Creates `mockito::Server` and function mapping `TileId` to this
    /// server's URL.
    fn mockito_server() -> (mockito::ServerGuard, Source) {
        let server = mockito::Server::new();
        let url = server.url();

        let source = move |tile_id: TileId| {
            format!("{}/{}/{}/{}.png", url, tile_id.zoom, tile_id.x, tile_id.y)
        };

        (server, Box::new(source))
    }

    #[test]
    fn download_single_tile() {
        let _ = env_logger::try_init();

        let (mut server, source) = mockito_server();
        let tile_mock = server
            .mock("GET", "/3/1/2.png")
            .with_body(include_bytes!("valid.png"))
            .create();

        let mut tiles = Tiles::new(source, Context::default());

        // First query start the download, but it will always return None.
        assert!(tiles.at(TILE_ID).is_none());

        // Eventually it gets downloaded and become available in cache.
        while tiles.at(TILE_ID).is_none() {}

        tile_mock.assert();
    }

    fn assert_tile_is_empty_forever(tiles: &mut Tiles) {
        // Should be None now, and forever.
        assert!(tiles.at(TILE_ID).is_none());
        std::thread::sleep(Duration::from_secs(1));
        assert!(tiles.at(TILE_ID).is_none());
    }

    #[test]
    fn tile_is_empty_forever_if_http_returns_error() {
        let _ = env_logger::try_init();

        let (mut server, source) = mockito_server();
        let mut tiles = Tiles::new(source, Context::default());
        let tile_mock = server.mock("GET", "/3/1/2.png").with_status(404).create();

        assert_tile_is_empty_forever(&mut tiles);
        tile_mock.assert();
    }

    #[test]
    fn tile_is_empty_forever_if_http_returns_no_body() {
        let _ = env_logger::try_init();

        let (mut server, source) = mockito_server();
        let mut tiles = Tiles::new(source, Context::default());
        let tile_mock = server.mock("GET", "/3/1/2.png").create();

        assert_tile_is_empty_forever(&mut tiles);
        tile_mock.assert();
    }

    #[test]
    fn tile_is_empty_forever_if_http_returns_garbage() {
        let _ = env_logger::try_init();

        let (mut server, source) = mockito_server();
        let mut tiles = Tiles::new(source, Context::default());
        let tile_mock = server
            .mock("GET", "/3/1/2.png")
            .with_body("definitely not an image")
            .create();

        assert_tile_is_empty_forever(&mut tiles);
        tile_mock.assert();
    }

    #[test]
    fn tile_is_empty_forever_if_http_can_not_even_connect() {
        let _ = env_logger::try_init();

        let source = |_| "totally invalid url".to_string();
        let mut tiles = Tiles::new(source, Context::default());

        assert_tile_is_empty_forever(&mut tiles);
    }
}
