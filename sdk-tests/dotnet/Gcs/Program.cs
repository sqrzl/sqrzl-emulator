using System.Collections.Concurrent;
using System.Net;
using System.Text;
using System.Text.Json;
using Google.Apis.Auth.OAuth2;
using Google.Apis.Http;
using Google.Cloud.Storage.V1;
using Bucket = Google.Apis.Storage.v1.Data.Bucket;

var endpoint = new Uri(Environment.GetEnvironmentVariable("GCS_ENDPOINT") ?? throw new InvalidOperationException("GCS_ENDPOINT is required"));
var token = Environment.GetEnvironmentVariable("GCS_BEARER_TOKEN");
var transport = new ObservedFactory(endpoint);
var builder = new StorageClientBuilder {
    BaseUri = new Uri(endpoint, "storage/v1/").ToString(),
    UnauthenticatedAccess = string.IsNullOrEmpty(token),
    HttpClientFactory = transport,
};
if (!string.IsNullOrEmpty(token)) builder.Credential = GoogleCredential.FromAccessToken(token);
var client = await builder.BuildAsync();
var name = "dotnet-" + Guid.NewGuid().ToString("N");
var key = "projects/p/locations/us/keyRings/ring/cryptoKeys/dotnet";
var created = await client.CreateBucketAsync("local-project", new Bucket {
    Name = name, Encryption = new Bucket.EncryptionData { DefaultKmsKeyName = key },
});
Require(created.Name == name, "create bucket identity");
var fetched = await client.GetBucketAsync(name);
Require(fetched.Encryption?.DefaultKmsKeyName == key, "bucket default KMS metadata");
var bytes = Encoding.UTF8.GetBytes("Google .NET default gzip and resumable upload regression");
using var source = new MemoryStream(bytes);
var uploaded = await client.UploadObjectAsync(name, "folder/value.txt", "text/plain", source);
Require(uploaded.Name == "folder/value.txt", "uploaded object identity");
Require(uploaded.KmsKeyName == key + "/cryptoKeyVersions/1", "default object KMS metadata");
using var downloaded = new MemoryStream();
await client.DownloadObjectAsync(name, uploaded.Name, downloaded);
Require(downloaded.ToArray().SequenceEqual(bytes), "complete byte readback");
await client.DeleteObjectAsync(name, uploaded.Name);
try {
    await client.GetObjectAsync(name, uploaded.Name);
    throw new InvalidOperationException("missing object request succeeded");
} catch (Google.GoogleApiException exception) when (exception.HttpStatusCode == HttpStatusCode.NotFound) { }
await client.DeleteBucketAsync(name);
Require(transport.GzipDefault, "official SDK default gzip setting");
Require(transport.Requests.Any(request => request.Method == "POST" && request.Gzip && request.Path.StartsWith("/storage/v1/b?")), "gzip-encoded bucket metadata reached the transport");
Require(transport.Requests.Any(request => request.Method == "POST" && request.Path.Contains("uploadType=resumable") && request.Status == 200), "resumable initiation");
Require(transport.Requests.Any(request => request.Method == "PUT" && request.Path.StartsWith("/upload/resumable/") && request.Status == 200), "resumable completion");
Console.WriteLine(JsonSerializer.Serialize(new {
    sdk = "Google.Cloud.Storage.V1", version = "5.0.0", gzip_default = transport.GzipDefault,
    assertions = new[] { "gzip-bucket-create", "bucket-default-kms", "resumable-upload", "object-kms", "full-byte-readback", "native-not-found", "delete-cleanup" },
    requests = transport.Requests.ToArray(),
}));

static void Require(bool valid, string name) {
    if (!valid) throw new InvalidOperationException("Failed assertion: " + name);
}

sealed record ObservedRequest(string Method, string Path, bool Gzip, int Status);

sealed class ObservedFactory(Uri endpoint) : HttpClientFactory {
    public ConcurrentQueue<ObservedRequest> Requests { get; } = new();
    public bool GzipDefault { get; private set; }
    protected override HttpMessageHandler CreateHandler(CreateHttpClientArgs args) {
        GzipDefault = args.GZipEnabled;
        return new Observer(endpoint, Requests) { InnerHandler = base.CreateHandler(args) };
    }
}

sealed class Observer(Uri endpoint, ConcurrentQueue<ObservedRequest> requests) : DelegatingHandler {
    protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken) {
        var target = request.RequestUri ?? throw new InvalidOperationException("Missing request URI");
        if (target.Scheme != endpoint.Scheme || target.Host != endpoint.Host || target.Port != endpoint.Port)
            throw new InvalidOperationException("The SDK attempted to leave the configured emulator endpoint");
        var gzip = request.Content?.Headers.ContentEncoding.Contains("gzip") == true;
        var response = await base.SendAsync(request, cancellationToken);
        requests.Enqueue(new(request.Method.Method, target.PathAndQuery, gzip, (int)response.StatusCode));
        return response;
    }
}
