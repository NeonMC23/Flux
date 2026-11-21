{
  "schema_version": 1,
  "kind": "flux-catalog",
  "library_id": "0123456789abcdef0123456789abcdef",
  "generation": 1,
  "next_track_id": 2,
  "next_artist_id": 3,
  "next_album_id": 3,
  "next_playlist_id": 1,
  "artists": [
    {
      "id": 1,
      "path": "artists/artist_000001"
    },
    {
      "id": 2,
      "path": "artists/artist_000002"
    }
  ],
  "albums": [
    {
      "id": 1,
      "artist_id": 1,
      "path": "artists/artist_000001/albums/album_000001"
    },
    {
      "id": 2,
      "artist_id": 2,
      "path": "artists/artist_000002/albums/album_000002"
    }
  ],
  "tracks": [
    {
      "id": 1,
      "owner_artist_id": 1,
      "album_id": 1,
      "audio_path": "artists/artist_000001/albums/album_000001/tracks/track_000001.wav",
      "record_path": "artists/artist_000001/albums/album_000001/tracks/track_000001.flux"
    }
  ],
  "playlists": [],
  "ratings_path": "ratings.flux"
}
