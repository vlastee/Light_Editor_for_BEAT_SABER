using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Text;
using UnityEngine;
using UnityEngine.SceneManagement;

/// <summary>
/// Comparison probe for bs-light-timeline. With PROBE_OUT=&lt;dir&gt; (and a map opened by
/// ChroMapper-PropPreview's scripted run), it writes:
/// - environment.json: the loaded environment's meshes in world space (metres, ChroMapper units
///   x 0.6), each light with its event type and light ID, and the environment colors;
/// - lights.jsonl: every light's color each time the editor time settles on a new beat.
/// </summary>
[Plugin("BslProbe")]
public class Probe
{
    [Init]
    private void Init()
    {
        if (string.IsNullOrEmpty(System.Environment.GetEnvironmentVariable("PROBE_OUT"))) return;
        SceneManager.sceneLoaded += (scene, mode) =>
        {
            if (scene.buildIndex == 3 && mode == LoadSceneMode.Single)
                new GameObject("BslProbe").AddComponent<ProbeRunner>();
        };
    }
}

public class ProbeRunner : MonoBehaviour
{
    private const float Unit = 0.6f;
    private string outDir;
    private BeatmapRuntimeContext context;
    private float lastTime = float.NaN;
    private int stableFrames;
    private readonly HashSet<float> recorded = new();
    private bool exported;

    // Map light IDs (as written in beatmaps) that reach each light. ChroMapper registers lights under an
    // internal index and sends an event's light ID k to the light at index remap[k], or at index k when k is
    // not remapped (GetLightControllerFromLightIds). A light can be reached by several IDs, or by none.
    private readonly Dictionary<LightController, List<int>> mapKeys = new();

    private static string F(float v) => v.ToString("0.####", CultureInfo.InvariantCulture);

    // Controllers a lane actually drives (its private lightIDToController); others keep their
    // initial color and are exported as static objects.
    private readonly HashSet<LightController> registered = new();

    private void BuildKeys()
    {
        mapKeys.Clear();
        registered.Clear();
        const System.Reflection.BindingFlags flags =
            System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Instance;
        var byIdField = typeof(BasicLightEffect).GetField("lightIDToController", flags);
        var remapField = typeof(BasicLightEffect).GetField("lightIdRemap", flags);
        foreach (var e in FindObjectsByType<BasicLightEffect>(FindObjectsSortMode.None))
        {
            if (byIdField?.GetValue(e) is not Dictionary<int, LightController> byId) continue;
            var remap = remapField?.GetValue(e) as Dictionary<int, int> ?? new Dictionary<int, int>();
            foreach (var kv in byId)
            {
                var (id, c) = (kv.Key, kv.Value);
                registered.Add(c);
                if (!mapKeys.ContainsKey(c)) mapKeys[c] = new List<int>();
                if (!remap.ContainsKey(id)) mapKeys[c].Add(id);
            }
            foreach (var kv in remap)
                if (byId.TryGetValue(kv.Value, out var c))
                    mapKeys[c].Add(kv.Key);
        }
        foreach (var keys in mapKeys.Values) keys.Sort();
    }

    private List<int> Keys(LightController l) => mapKeys.TryGetValue(l, out var k) ? k : new List<int>();

    // A single ID as a number, otherwise the list.
    private string KeysJson(LightController l)
    {
        var k = Keys(l);
        return k.Count == 1 ? k[0].ToString(CultureInfo.InvariantCulture) : "[" + string.Join(",", k) + "]";
    }

    // LightSink: ChroMapper's placeholder controller that swallows colors and never draws.
    private bool Driven(LightController l) => l != null && l is not LightSink && registered.Contains(l);
    private static string C(Color c) => $"[{F(c.r)},{F(c.g)},{F(c.b)},{F(c.a)}]";

    private void Start() => outDir = System.Environment.GetEnvironmentVariable("PROBE_OUT");

    private void Update()
    {
        if (context == null)
        {
            context = FindAnyObjectByType<BeatmapRuntimeContext>();
            return;
        }
        if (context.Descriptor == null || context.Atsc == null) return;
        if (!exported && Time.timeSinceLevelLoad > 3f)
        {
            exported = true;
            BuildKeys();
            Export();
        }
        var t = context.Atsc.CurrentJsonTime;
        if (t != lastTime) { lastTime = t; stableFrames = 0; return; }
        if (++stableFrames == 3 && exported && recorded.Add(t)) Record(t);
    }

    private void Record(float beat)
    {
        var sb = new StringBuilder();
        sb.Append("{\"beat\":").Append(F(beat)).Append(",\"lights\":[");
        var first = true;
        foreach (var l in FindObjectsByType<LightController>(FindObjectsSortMode.None))
        {
            if (!Driven(l)) continue;
            if (!first) sb.Append(',');
            first = false;
            sb.Append('[').Append(l.Type).Append(',').Append(KeysJson(l)).Append(',').Append(C(l.Color)).Append(']');
        }
        sb.Append("]}\n");
        File.AppendAllText(Path.Combine(outDir, "lights.jsonl"), sb.ToString());
    }

    private void Export()
    {
        var d = context.Descriptor;
        var scheme = d.ColorSchemeProvider != null ? d.ColorSchemeProvider.ColorScheme : null;
        var sb = new StringBuilder();
        sb.Append("{\"name\":\"").Append(d.ID).Append('"');
        if (scheme != null)
            sb.Append(",\"red\":").Append(C(scheme.EnvironmentLeftColor)).Append(",\"blue\":").Append(C(scheme.EnvironmentRightColor))
              .Append(",\"white\":").Append(C(scheme.EnvironmentWhiteColor))
              .Append(",\"redBoost\":").Append(C(scheme.EnvironmentLeftBoostColor))
              .Append(",\"blueBoost\":").Append(C(scheme.EnvironmentRightBoostColor))
              .Append(",\"whiteBoost\":").Append(C(scheme.EnvironmentWhiteBoostColor));
        sb.Append(",\"lanes\":[");
        var firstLane = true;
        foreach (var e in FindObjectsByType<BasicLightEffect>(FindObjectsSortMode.None).OrderBy(e => e.ID))
        {
            if (!firstLane) sb.Append(',');
            firstLane = false;
            sb.Append("{\"type\":").Append(e.ID).Append(",\"off\":").Append(F(e.OffIntensity))
              .Append(",\"onStart\":").Append(e.LightOnStart ? "true" : "false")
              .Append(",\"invert\":").Append(e.InvertColorScheme ? "true" : "false")
              .Append(",\"highlightMatches\":").Append(e.HighlightMatchesNormalColor ? "true" : "false").Append('}');
        }
        sb.Append(']');
        sb.Append(",\"objects\":[");
        int unreadable = 0, total = 0;
        var first = true;
        foreach (var root in d.gameObject.scene.GetRootGameObjects())
        {
            foreach (var r in root.GetComponentsInChildren<Renderer>(false))
            {
                var tris = Triangles(r, out var readable);
                if (tris == null) continue;
                total++;
                if (!readable) unreadable++;
                var light = r.GetComponentInParent<LightController>();
                if (!Driven(light)) light = null;
                var mat = r.sharedMaterial;
                var col = Color.gray;
                if (mat != null)
                {
                    if (mat.HasProperty("_Color")) col = mat.GetColor("_Color");
                    else if (mat.HasProperty("_BaseColor")) col = mat.GetColor("_BaseColor");
                }
                if (!first) sb.Append(',');
                first = false;
                sb.Append("{\"light\":").Append(light != null ? $"[{light.Type},{KeysJson(light)}]" : "null")
                  .Append(",\"color\":").Append(C(col))
                  .Append(",\"name\":\"").Append(r.gameObject.name.Replace("\"", "'")).Append('"')
                  .Append(",\"tris\":[").Append(string.Join(",", tris.Select(F))).Append("]}");
            }
        }
        sb.Append("],\"stats\":{\"renderers\":").Append(total).Append(",\"unreadable\":").Append(unreadable).Append("}}");
        File.WriteAllText(Path.Combine(outDir, "environment.json"), sb.ToString());
        if (context.EnvironmentList != null)
            File.WriteAllLines(Path.Combine(outDir, "environments.txt"),
                context.EnvironmentList.List.Where(x => !x.Ignore).Select(x => x.ID));
    }

    /// World-space triangle soup (metres), or null for renderers we don't export.
    private static List<float> Triangles(Renderer r, out bool readable)
    {
        readable = true;
        var m = r.localToWorldMatrix;
        var verts = new List<Vector3>();
        var idx = new List<int>();
        switch (r)
        {
            case SpriteRenderer sr when sr.sprite != null:
                verts.AddRange(sr.sprite.vertices.Select(v => (Vector3)v));
                idx.AddRange(sr.sprite.triangles.Select(i => (int)i));
                break;
            case MeshRenderer or SkinnedMeshRenderer:
                var mesh = r is SkinnedMeshRenderer smr ? smr.sharedMesh : r.GetComponent<MeshFilter>()?.sharedMesh;
                if (mesh == null) return null;
                if (mesh.isReadable)
                {
                    mesh.GetVertices(verts);
                    for (var s = 0; s < mesh.subMeshCount; s++)
                        if (mesh.GetTopology(s) == MeshTopology.Triangles) idx.AddRange(mesh.GetTriangles(s));
                }
                else
                {
                    // Not readable in a build: use its bounding box.
                    readable = false;
                    var b = mesh.bounds;
                    for (var i = 0; i < 8; i++)
                        verts.Add(b.center + Vector3.Scale(b.extents, new Vector3((i & 1) * 2 - 1, (i >> 1 & 1) * 2 - 1, (i >> 2 & 1) * 2 - 1)));
                    idx.AddRange(new[] { 0,1,3, 0,3,2, 4,6,7, 4,7,5, 0,4,5, 0,5,1, 2,3,7, 2,7,6, 0,2,6, 0,6,4, 1,5,7, 1,7,3 });
                }
                break;
            default:
                return null;
        }
        var outv = new List<float>(idx.Count * 3);
        foreach (var i in idx)
        {
            if (i < 0 || i >= verts.Count) continue;
            var w = m.MultiplyPoint3x4(verts[i]) * Unit;
            outv.Add(w.x); outv.Add(w.y); outv.Add(w.z);
        }
        return outv;
    }
}
