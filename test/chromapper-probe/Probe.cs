using System.Collections;
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

    private void Start()
    {
        outDir = System.Environment.GetEnvironmentVariable("PROBE_OUT");
        if (Full) Camera.onPostRender += SaveFogTexture;
    }

    private void OnDestroy() => Camera.onPostRender -= SaveFogTexture;

    private int fogShots;

    // During PropPreview's pose shots (the editor camera rendering into a texture): the fog texture
    // that frame used, as a linear PNG, in shot order.
    private void SaveFogTexture(Camera cam)
    {
        if (cam.targetTexture == null || cam.name != "MapEditor Camera") return;
        var fog = Shader.GetGlobalTexture("_BloomPrePassTexture");
        if (fog == null) return;
        var rt = RenderTexture.GetTemporary(fog.width, fog.height, 0, RenderTextureFormat.ARGB32, RenderTextureReadWrite.Linear);
        var prev = RenderTexture.active;
        Graphics.Blit(fog, rt);
        RenderTexture.active = rt;
        var img = new Texture2D(fog.width, fog.height, TextureFormat.RGB24, false, true);
        img.ReadPixels(new Rect(0, 0, fog.width, fog.height), 0, 0);
        img.Apply();
        RenderTexture.active = prev;
        RenderTexture.ReleaseTemporary(rt);
        File.WriteAllBytes(Path.Combine(outDir, $"fog-shot{fogShots++}.png"), img.EncodeToPNG());
        Destroy(img);
        var ratio = Shader.GetGlobalVector("_CustomFogTextureToScreenRatio");
        File.AppendAllText(Path.Combine(outDir, "fog-shots.txt"), $"{fogShots - 1} {F(context.Atsc.CurrentJsonTime)} {F(ratio.x)} {F(ratio.y)} {cam.fieldOfView} {cam.pixelWidth}x{cam.pixelHeight}\n");
    }

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
        sb.Append("],\"moved\":[");
        // Objects whose transform changed since the export (rotating lasers, rings): index and world matrix.
        var firstMoved = true;
        for (var i = 0; i < exportedRenderers.Count; i++)
        {
            var r = exportedRenderers[i];
            if (r == null) continue;
            var m = r.localToWorldMatrix;
            if (m == exportedMatrices[i]) continue;
            if (!firstMoved) sb.Append(',');
            firstMoved = false;
            sb.Append('[').Append(i).Append(',').Append(M(m)).Append(']');
        }
        sb.Append("],\"unityLights\":[").Append(string.Join(",", FindObjectsByType<Light>(FindObjectsSortMode.None)
            .Select(l => "[\"" + Esc(l.name) + "\"," + C(l.color) + "," + F(l.intensity) + "]"))).Append("]}\n");
        File.AppendAllText(Path.Combine(outDir, "lights.jsonl"), sb.ToString());
        if (Full) RecordFull(beat);
    }

    private static bool Full => !string.IsNullOrEmpty(System.Environment.GetEnvironmentVariable("PROBE_FULL"));

    // Generic field dump (public and private instance fields) for the renderer port.
    private static string Dump(object o, int depth)
    {
        switch (o)
        {
            case null: return "null";
            case float f: return float.IsFinite(f) ? F(f) : "null";
            case double dd: return double.IsFinite(dd) ? F((float)dd) : "null";
            case int i: return i.ToString(CultureInfo.InvariantCulture);
            case uint ui: return ui.ToString(CultureInfo.InvariantCulture);
            case long l: return l.ToString(CultureInfo.InvariantCulture);
            case bool b: return b ? "true" : "false";
            case string str: return "\"" + Esc(str) + "\"";
            case System.Enum e: return "\"" + e + "\"";
            case Color c: return C(c);
            case Color32 c32: return C(c32);
            case Vector2 v2: return $"[{F(v2.x)},{F(v2.y)}]";
            case Vector3 v3: return $"[{F(v3.x)},{F(v3.y)},{F(v3.z)}]";
            case Vector4 v4: return V4(v4);
            case Quaternion q: return $"[{F(q.x)},{F(q.y)},{F(q.z)},{F(q.w)}]";
            case Matrix4x4 mm: return M(mm);
            case AnimationCurve ac:
                return "[" + string.Join(",", ac.keys.Select(k => $"[{F(k.time)},{F(k.value)},{F(k.inTangent)},{F(k.outTangent)}]")) + "]";
            case Material mat: return mat == null ? "null" : "{\"material\":\"" + Esc(mat.name) + "\",\"shader\":\"" + Esc(mat.shader != null ? mat.shader.name : "") + "\"}";
            case Transform t: return t == null ? "null" : "{\"path\":\"" + Esc(PathOf(t)) + "\",\"m\":" + M(t.localToWorldMatrix) + "}";
            case Component comp: return comp == null ? "null" : "{\"component\":\"" + comp.GetType().Name + "\",\"id\":" + comp.GetInstanceID() + ",\"path\":\"" + Esc(PathOf(comp.transform)) + "\"}";
            case Texture tex: return tex == null ? "null" : "\"" + TexKey(tex) + "\"";
            case Object uo: return uo == null ? "null" : "\"" + Esc(uo.name) + "\"";
            case System.Delegate: return "null";
            case IList list:
                if (depth <= 0) return "\"list\"";
                return "[" + string.Join(",", list.Cast<object>().Take(512).Select(x => Dump(x, depth - 1))) + "]";
        }
        var ty = o.GetType();
        if (depth <= 0 || ty.IsPointer) return "\"" + ty.Name + "\"";
        return Fields(o, depth - 1);
    }

    private static string Fields(object o, int depth)
    {
        const System.Reflection.BindingFlags all = System.Reflection.BindingFlags.Public
            | System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Instance;
        var parts = new List<string>();
        for (var ty = o.GetType(); ty != null && ty != typeof(MonoBehaviour) && ty != typeof(object); ty = ty.BaseType)
            foreach (var f in ty.GetFields(all | System.Reflection.BindingFlags.DeclaredOnly))
            {
                if (typeof(System.Delegate).IsAssignableFrom(f.FieldType)) continue;
                object v;
                try { v = f.GetValue(o); } catch { continue; }
                parts.Add("\"" + f.Name + "\":" + Dump(v, depth));
            }
        return "{" + string.Join(",", parts) + "}";
    }

    private static string PathOf(Transform t)
    {
        var path = t.name;
        for (var p = t.parent; p != null; p = p.parent) path = p.name + "/" + path;
        return path;
    }

    private static readonly System.Text.RegularExpressions.Regex componentTypes = new(
        "Light|Bloom|Glow|Reflection|Spectrogram|Mirror|Ring|Rotation|Gradient|Lightmap|Fog|Sprite|Material|Parametric|Probe",
        System.Text.RegularExpressions.RegexOptions.Compiled);

    private readonly List<BloomFogObject> bloomFogExported = new();

    private void ExportFull(EnvironmentDescriptor d)
    {
        // Components that drive lighting, fog and reflections.
        var sb = new StringBuilder("[");
        var first = true;
        foreach (var root in d.gameObject.scene.GetRootGameObjects())
            foreach (var mb in root.GetComponentsInChildren<MonoBehaviour>(true))
            {
                if (mb == null || !componentTypes.IsMatch(mb.GetType().Name)) continue;
                if (!first) sb.Append(",\n");
                first = false;
                var lc = mb as LightController ?? mb.GetComponentInParent<LightController>();
                sb.Append("{\"type\":\"").Append(mb.GetType().Name).Append("\",\"id\":").Append(mb.GetInstanceID())
                  .Append(",\"path\":\"").Append(Esc(PathOf(mb.transform)))
                  .Append("\",\"active\":").Append(mb.isActiveAndEnabled ? "true" : "false")
                  .Append(",\"light\":").Append(Driven(lc) ? $"[{lc.Type},{KeysJson(lc)}]" : "null")
                  .Append(",\"m\":").Append(M(mb.transform.localToWorldMatrix))
                  .Append(",\"fields\":").Append(Fields(mb, 2)).Append('}');
            }
        sb.Append("]");
        File.WriteAllText(Path.Combine(outDir, "components.json"), sb.ToString());

        // BloomFogObjects: static settings, owning light and material.
        bloomFogExported.Clear();
        bloomFogExported.AddRange(BloomFogObject.AllBloomFogLights.Where(b => b != null));
        var bf = new StringBuilder("[");
        for (var i = 0; i < bloomFogExported.Count; i++)
        {
            var b = bloomFogExported[i];
            var lc = b.GetComponentInParent<LightController>();
            if (i > 0) bf.Append(",\n");
            bf.Append("{\"id\":").Append(b.GetInstanceID()).Append(",\"path\":\"").Append(Esc(PathOf(b.transform))).Append("\",\"light\":")
              .Append(Driven(lc) ? $"[{lc.Type},{KeysJson(lc)}]" : "null")
              .Append(",\"material\":").Append(b.LightType?.Material != null ? "\"" + Esc(b.LightType.Material.name) + "\"" : "null")
              .Append(",\"blendOp\":").Append(b.LightType?.Material != null && b.LightType.Material.HasProperty("_BlendOp") ? F(b.LightType.Material.GetFloat("_BlendOp")) : "null")
              .Append(",\"priority\":").Append(b.LightType?.RenderingPriority ?? 0)
              .Append(",\"fields\":").Append(Fields(b, 1)).Append('}');
            if (b.LightType?.Material != null && b.LightType.Material.HasProperty("_MainTex") && b.LightType.Material.GetTexture("_MainTex") != null)
                TexKey(b.LightType.Material.GetTexture("_MainTex"));
        }
        bf.Append("]");
        File.WriteAllText(Path.Combine(outDir, "bloomfog.json"), bf.ToString());

        // Textures as PNG (their stored values: sRGB textures stay encoded).
        var dir = Path.Combine(outDir, "textures");
        Directory.CreateDirectory(dir);
        var info = new List<string>();
        foreach (var kv in textureKeys.ToList())
        {
            var t = kv.Key;
            if (t == null) continue;
            var srgb = t is Texture2D t2 ? t2.isDataSRGB : false;
            info.Add($"\"{kv.Value}\":{{\"name\":\"{Esc(t.name)}\",\"w\":{t.width},\"h\":{t.height},\"dim\":\"{t.dimension}\",\"srgb\":{(srgb ? "true" : "false")},\"wrap\":\"{t.wrapMode}\",\"filter\":\"{t.filterMode}\"}}");
            if (t.dimension != UnityEngine.Rendering.TextureDimension.Tex2D) continue;
            try
            {
                var rt = RenderTexture.GetTemporary(t.width, t.height, 0, RenderTextureFormat.ARGB32,
                    srgb ? RenderTextureReadWrite.sRGB : RenderTextureReadWrite.Linear);
                Graphics.Blit(t, rt);
                var prev = RenderTexture.active;
                RenderTexture.active = rt;
                var img = new Texture2D(t.width, t.height, TextureFormat.RGBA32, false, !srgb);
                img.ReadPixels(new Rect(0, 0, t.width, t.height), 0, 0);
                img.Apply();
                RenderTexture.active = prev;
                RenderTexture.ReleaseTemporary(rt);
                File.WriteAllBytes(Path.Combine(dir, kv.Value + ".png"), img.EncodeToPNG());
                Destroy(img);
            }
            catch (System.Exception e) { Debug.LogWarning($"BslProbe: texture {t.name}: {e.Message}"); }
        }
        File.WriteAllText(Path.Combine(dir, "index.json"), "{" + string.Join(",\n", info) + "}");
    }

    private static readonly string[] globalFloats =
    {
        "_CustomFogOffset", "_CustomFogAttenuation", "_CustomFogHeightFogStartY", "_CustomFogHeightFogHeight",
        "_BaseColorBoost", "_BaseColorBoostThreshold", "_HasBakedReflectionProbeData",
    };
    private static readonly string[] globalVectors =
    {
        "_CustomFogTextureToScreenRatio", "_BloomParams", "_LightProbeLightBakeIdA", "_LightProbeLightBakeIdB",
        "_LightProbeLightBakeIdC", "_LightProbeLightBakeIdD", "_LightProbeLightBakeIdE", "_LightProbeLightBakeIdF",
        "_LightmapLightBakeIdA", "_LightmapLightBakeIdB", "_LightmapLightBakeIdC", "_LightmapLightBakeIdD",
        "_LightmapLightBakeIdE", "_LightmapLightBakeIdF", "_ReflectionProbePosition", "_ReflectionProbeBoundsMin",
        "_ReflectionProbeBoundsMax", "_GlobalLightTintColor", "_TimeHelperOffset",
    };
    private static readonly string[] blockNames =
    {
        "_Color", "_AlphaWidth", "_SizeParams", "_EmissionTexColor", "_EmissionColor", "_PrivatePointLightColor",
        "_ParallaxColor", "_TintColor", "_EmissionGradientIntensity", "_PeakOffset",
    };

    private void RecordFull(float beat)
    {
        var sb = new StringBuilder();
        sb.Append("{\"beat\":").Append(F(beat));
        sb.Append(",\"cameras\":[").Append(string.Join(",", Camera.allCameras.Where(c => c.isActiveAndEnabled).Select(c =>
            "{\"name\":\"" + Esc(c.name) + "\",\"depth\":" + F(c.depth) + ",\"target\":" + (c.targetTexture != null ? "true" : "false")
            + ",\"fov\":" + F(c.fieldOfView) + ",\"near\":" + F(c.nearClipPlane) + ",\"far\":" + F(c.farClipPlane)
            + ",\"aspect\":" + F(c.aspect) + ",\"rect\":" + V4(new Vector4(c.pixelRect.x, c.pixelRect.y, c.pixelRect.width, c.pixelRect.height))
            + ",\"m\":" + M(c.transform.localToWorldMatrix) + ",\"proj\":" + M(c.projectionMatrix)
            + ",\"bg\":" + C(c.backgroundColor) + "}"))).Append(']');
        sb.Append(",\"globals\":{");
        var g = new List<string>();
        foreach (var n in globalFloats) g.Add("\"" + n + "\":" + F(Shader.GetGlobalFloat(n)));
        foreach (var n in globalVectors) g.Add("\"" + n + "\":" + V4(Shader.GetGlobalVector(n)));
        foreach (var n in new[] { "_DirectionalLightPositions", "_DirectionalLightDirections", "_DirectionalLightColors" })
        {
            var arr = Shader.GetGlobalVectorArray(n);
            g.Add("\"" + n + "\":" + (arr == null ? "null" : "[" + string.Join(",", arr.Select(V4)) + "]"));
        }
        var radii = Shader.GetGlobalFloatArray("_DirectionalLightRadii");
        g.Add("\"_DirectionalLightRadii\":" + (radii == null ? "null" : "[" + string.Join(",", radii.Select(F)) + "]"));
        foreach (var kw in new[] { "BLOOM_FOG", "ACES_TONE_MAPPING", "POST_BLOOM" })
            g.Add("\"" + kw + "\":" + (Shader.IsKeywordEnabled(kw) ? "true" : "false"));
        sb.Append(string.Join(",", g)).Append('}');
        // BloomFogObjects: transform, color and enabled state.
        var colorField = typeof(BloomFogObject).GetField("color",
            System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Instance);
        sb.Append(",\"bloomFog\":[").Append(string.Join(",", bloomFogExported.Select(b =>
            b == null ? "null" : "[" + (b.isActiveAndEnabled ? "1" : "0") + "," + C((Color)colorField.GetValue(b)) + ","
                + M(b.transform.localToWorldMatrix) + "," + F(b.Length) + "," + F(b.StartAlpha) + "," + F(b.EndAlpha) + ","
                + F(b.StartWidth) + "," + F(b.EndWidth) + "," + F(b.MultiplyLengthByAlphaBloomFogMultiplier) + ","
                + F(b.MultiplyLengthByAlphaMultiplier) + "]"))).Append(']');
        // Per-renderer property blocks (light colors), by export index.
        var mpb = new MaterialPropertyBlock();
        var blocks = new List<string>();
        for (var i = 0; i < exportedRenderers.Count; i++)
        {
            var r = exportedRenderers[i];
            if (r == null || !r.HasPropertyBlock()) continue;
            r.GetPropertyBlock(mpb);
            var props = new List<string>();
            foreach (var n in blockNames)
            {
                if (mpb.HasVector(n) || mpb.HasColor(n)) props.Add("\"" + n + "\":" + V4(mpb.GetVector(n)));
                else if (mpb.HasFloat(n)) props.Add("\"" + n + "\":" + F(mpb.GetFloat(n)));
            }
            blocks.Add("[" + i + "," + (r.enabled ? "1" : "0") + ",{" + string.Join(",", props) + "}]");
        }
        sb.Append(",\"blocks\":[").Append(string.Join(",", blocks)).Append(']');
        sb.Append(",\"particles\":[").Append(string.Join(",", particleSystems.Select(ParticlesJson))).Append("]}\n");
        File.AppendAllText(Path.Combine(outDir, "snapshots.jsonl"), sb.ToString());
    }

    // Export v2: shared meshes (indexed, mesh space) and materials; objects reference them with a world matrix.
    private readonly List<Renderer> exportedRenderers = new();
    private readonly List<Matrix4x4> exportedMatrices = new();

    private void Export()
    {
        var d = context.Descriptor;
        var scheme = d.ColorSchemeProvider != null ? d.ColorSchemeProvider.ColorScheme : null;
        var sb = new StringBuilder();
        sb.Append("{\"version\":2,\"unit\":").Append(F(Unit)).Append(",\"name\":\"").Append(d.ID).Append('"');
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
        sb.Append(",\"fog\":[").Append(string.Join(",", FogParams())).Append(']');
        sb.Append(",\"unityLights\":[").Append(string.Join(",", UnityLights())).Append(']');

        var meshIndex = new Dictionary<Object, int>();
        var matIndex = new Dictionary<Material, int>();
        var meshes = new List<string>();
        var mats = new List<string>();
        var objs = new List<string>();
        int unreadable = 0;
        exportedRenderers.Clear();
        exportedMatrices.Clear();
        var systems = new List<string>();
        particleSystems.Clear();
        foreach (var root in d.gameObject.scene.GetRootGameObjects())
        {
            foreach (var r in root.GetComponentsInChildren<Renderer>(false))
            {
                if (r is ParticleSystemRenderer psr)
                {
                    var ps = psr.GetComponent<ParticleSystem>();
                    if (ps == null) continue;
                    var pmats = new List<int>();
                    foreach (var mat in psr.sharedMaterials)
                    {
                        if (mat == null) { pmats.Add(-1); continue; }
                        if (!matIndex.TryGetValue(mat, out var k))
                        {
                            k = matIndex[mat] = mats.Count;
                            mats.Add(MaterialJson(mat));
                        }
                        pmats.Add(k);
                    }
                    particleSystems.Add(ps);
                    var pmesh = -1;
                    if (psr.renderMode == ParticleSystemRenderMode.Mesh && psr.mesh != null)
                    {
                        if (!meshIndex.TryGetValue(psr.mesh, out pmesh))
                        {
                            var json = MeshJsonOf(psr.mesh, out var okm);
                            if (json != null)
                            {
                                if (!okm) unreadable++;
                                pmesh = meshIndex[psr.mesh] = meshes.Count;
                                meshes.Add(json);
                            }
                            else pmesh = -1;
                        }
                    }
                    var main = ps.main;
                    var sheet = ps.textureSheetAnimation;
                    systems.Add("{\"path\":\"" + Esc(PathOf(r.transform)) + "\",\"materials\":[" + string.Join(",", pmats) + "]"
                        + ",\"m\":" + M(r.transform.localToWorldMatrix)
                        + ",\"space\":\"" + main.simulationSpace + "\",\"scaling\":\"" + main.scalingMode + "\""
                        + ",\"mesh\":" + pmesh
                        + ",\"renderMode\":\"" + psr.renderMode + "\",\"alignment\":\"" + psr.alignment + "\""
                        + ",\"sortMode\":\"" + psr.sortMode + "\",\"pivot\":" + Dump(psr.pivot, 0)
                        + ",\"minSize\":" + F(psr.minParticleSize) + ",\"maxSize\":" + F(psr.maxParticleSize)
                        + ",\"flip\":" + Dump(psr.flip, 0)
                        + ",\"sheet\":" + (sheet.enabled ? $"[{sheet.numTilesX},{sheet.numTilesY}]" : "null")
                        + ",\"enabled\":" + (r.enabled ? "true" : "false")
                        + ",\"particles\":" + ParticlesJson(ps) + "}");
                    continue;
                }
                Object key = r is SpriteRenderer sr ? sr.sprite : r is SkinnedMeshRenderer smr ? smr.sharedMesh
                    : r.GetComponent<MeshFilter>()?.sharedMesh;
                if (key == null) continue;
                if (!meshIndex.TryGetValue(key, out var mi))
                {
                    var json = MeshJson(r, out var ok);
                    if (json == null) continue;
                    if (!ok) unreadable++;
                    mi = meshIndex[key] = meshes.Count;
                    meshes.Add(json);
                }
                var matIds = new List<int>();
                foreach (var mat in r.sharedMaterials)
                {
                    if (mat == null) { matIds.Add(-1); continue; }
                    if (!matIndex.TryGetValue(mat, out var k))
                    {
                        k = matIndex[mat] = mats.Count;
                        mats.Add(MaterialJson(mat));
                    }
                    matIds.Add(k);
                }
                var light = r.GetComponentInParent<LightController>();
                if (!Driven(light)) light = null;
                var path = r.transform.name;
                for (var t = r.transform.parent; t != null; t = t.parent) path = t.name + "/" + path;
                var m = r.localToWorldMatrix;
                exportedRenderers.Add(r);
                exportedMatrices.Add(m);
                objs.Add("{\"light\":" + (light != null ? $"[{light.Type},{KeysJson(light)}]" : "null")
                    + ",\"mesh\":" + mi + ",\"materials\":[" + string.Join(",", matIds) + "]"
                    + ",\"m\":" + M(m)
                    + ",\"enabled\":" + (r.enabled ? "true" : "false")
                    + ",\"layer\":" + r.gameObject.layer
                    + ",\"kind\":\"" + r.GetType().Name + "\""
                    + ",\"sortingOrder\":" + r.sortingOrder
                    + ",\"name\":\"" + Esc(r.gameObject.name) + "\",\"path\":\"" + Esc(path) + "\""
                    + ",\"block\":" + BlockJson(r) + "}");
            }
        }
        sb.Append(",\"meshes\":[").Append(string.Join(",", meshes)).Append(']');
        sb.Append(",\"materialList\":[").Append(string.Join(",", mats)).Append(']');
        sb.Append(",\"objects\":[").Append(string.Join(",", objs)).Append(']');
        sb.Append(",\"particleSystems\":[").Append(string.Join(",", systems)).Append(']');
        sb.Append(",\"stats\":{\"renderers\":").Append(objs.Count).Append(",\"meshes\":").Append(meshes.Count)
          .Append(",\"unreadable\":").Append(unreadable).Append("}}");
        File.WriteAllText(Path.Combine(outDir, "environment.json"), sb.ToString());
        if (Full) ExportFull(d);
        if (context.EnvironmentList != null)
            File.WriteAllLines(Path.Combine(outDir, "environments.txt"),
                context.EnvironmentList.List.Where(x => !x.Ignore).Select(x => x.ID));
    }

    private readonly List<ParticleSystem> particleSystems = new();
    private static ParticleSystem.Particle[] particleBuffer = new ParticleSystem.Particle[4096];

    // Live particles: position, size, rotation (degrees), color (0..1), as the renderer draws them.
    private static string ParticlesJson(ParticleSystem ps)
    {
        if (ps == null) return "[]";
        if (particleBuffer.Length < ps.main.maxParticles) particleBuffer = new ParticleSystem.Particle[ps.main.maxParticles];
        var n = ps.GetParticles(particleBuffer);
        var sb = new StringBuilder("[");
        for (var i = 0; i < n; i++)
        {
            var p = particleBuffer[i];
            var sz = p.GetCurrentSize3D(ps);
            var c = (Color)p.GetCurrentColor(ps);
            var r = p.rotation3D;
            if (i > 0) sb.Append(',');
            sb.Append('[').Append(F(p.position.x)).Append(',').Append(F(p.position.y)).Append(',').Append(F(p.position.z)).Append(',')
              .Append(F(sz.x)).Append(',').Append(F(sz.y)).Append(',').Append(F(sz.z)).Append(',')
              .Append(F(r.x)).Append(',').Append(F(r.y)).Append(',').Append(F(r.z)).Append(',')
              .Append(F(c.r)).Append(',').Append(F(c.g)).Append(',').Append(F(c.b)).Append(',').Append(F(c.a)).Append(',')
              .Append(F(p.remainingLifetime / Mathf.Max(p.startLifetime, 1e-6f))).Append(',').Append(p.randomSeed % 100000).Append(']');
        }
        return sb.Append(']').ToString();
    }

    private static string Esc(string s) => s.Replace("\\", "/").Replace("\"", "'");

    private static string M(Matrix4x4 m)
    {
        var v = new float[16];
        for (var r = 0; r < 4; r++)
            for (var c = 0; c < 4; c++)
                v[r * 4 + c] = m[r, c];
        return "[" + string.Join(",", v.Select(F)) + "]";
    }

    private static string V4(Vector4 v) => $"[{F(v.x)},{F(v.y)},{F(v.z)},{F(v.w)}]";

    // Every property of the material's shader, plus render state the shader reads from it.
    // Textures referenced by exported materials (full mode writes them as PNGs).
    private static readonly Dictionary<Texture, string> textureKeys = new();

    private static string TexKey(Texture t)
    {
        if (!textureKeys.TryGetValue(t, out var k))
        {
            k = new string(t.name.Select(ch => char.IsLetterOrDigit(ch) || ch == '_' || ch == '-' ? ch : '_').ToArray())
                + "_" + (t.GetInstanceID() & 0x7fffffff);
            textureKeys[t] = k;
        }
        return k;
    }

    private static string MaterialJson(Material mat)
    {
        var sh = mat.shader;
        var props = new List<string>();
        var colors = new List<string>();
        if (sh != null)
        {
            for (var i = 0; i < sh.GetPropertyCount(); i++)
            {
                var n = sh.GetPropertyName(i);
                if (sh.GetPropertyType(i) == UnityEngine.Rendering.ShaderPropertyType.Color) colors.Add("\"" + n + "\"");
                string v;
                switch (sh.GetPropertyType(i))
                {
                    case UnityEngine.Rendering.ShaderPropertyType.Color: v = C(mat.GetColor(n)); break;
                    case UnityEngine.Rendering.ShaderPropertyType.Vector: v = V4(mat.GetVector(n)); break;
                    case UnityEngine.Rendering.ShaderPropertyType.Float:
                    case UnityEngine.Rendering.ShaderPropertyType.Range: v = F(mat.GetFloat(n)); break;
                    case UnityEngine.Rendering.ShaderPropertyType.Int: v = mat.GetInteger(n).ToString(CultureInfo.InvariantCulture); break;
                    case UnityEngine.Rendering.ShaderPropertyType.Texture:
                        var tex = mat.GetTexture(n);
                        v = tex == null ? "null" : "\"" + TexKey(tex) + "\"";
                        var st = new Vector4(mat.GetTextureScale(n).x, mat.GetTextureScale(n).y,
                            mat.GetTextureOffset(n).x, mat.GetTextureOffset(n).y);
                        props.Add("\"" + n + "_ST\":" + V4(st));
                        break;
                    default: continue;
                }
                props.Add("\"" + n + "\":" + v);
            }
        }
        return "{\"name\":\"" + Esc(mat.name) + "\",\"shader\":\"" + Esc(sh != null ? sh.name : "") + "\""
            + ",\"queue\":" + mat.renderQueue
            + ",\"keywords\":[" + string.Join(",", mat.shaderKeywords.Select(k => "\"" + k + "\"")) + "]"
            + ",\"colors\":[" + string.Join(",", colors) + "]"
            + ",\"props\":{" + string.Join(",", props) + "}}";
    }

    // Per-renderer overrides (lights set their color this way).
    private static string BlockJson(Renderer r)
    {
        if (!r.HasPropertyBlock()) return "null";
        var b = new MaterialPropertyBlock();
        r.GetPropertyBlock(b);
        var mat = r.sharedMaterial;
        var props = new List<string>();
        if (mat != null && mat.shader != null)
        {
            var sh = mat.shader;
            for (var i = 0; i < sh.GetPropertyCount(); i++)
            {
                var n = sh.GetPropertyName(i);
                switch (sh.GetPropertyType(i))
                {
                    case UnityEngine.Rendering.ShaderPropertyType.Color:
                        if (b.HasColor(n)) props.Add("\"" + n + "\":" + C(b.GetColor(n)));
                        break;
                    case UnityEngine.Rendering.ShaderPropertyType.Vector:
                        if (b.HasVector(n)) props.Add("\"" + n + "\":" + V4(b.GetVector(n)));
                        break;
                    case UnityEngine.Rendering.ShaderPropertyType.Float:
                    case UnityEngine.Rendering.ShaderPropertyType.Range:
                        if (b.HasFloat(n)) props.Add("\"" + n + "\":" + F(b.GetFloat(n)));
                        break;
                }
            }
        }
        return "{" + string.Join(",", props) + "}";
    }

    private IEnumerable<string> FogParams()
    {
        const System.Reflection.BindingFlags all = System.Reflection.BindingFlags.Public
            | System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Instance;
        foreach (var mb in FindObjectsByType<MonoBehaviour>(FindObjectsSortMode.None))
        {
            if (mb == null) continue;
            foreach (var f in mb.GetType().GetFields(all))
            {
                if (f.FieldType != typeof(BloomFogParams)) continue;
                if (f.GetValue(mb) is not BloomFogParams p) continue;
                yield return $"{{\"owner\":\"{mb.GetType().Name}\",\"object\":\"{Esc(mb.name)}\",\"field\":\"{f.Name}\""
                    + $",\"offset\":{F(p.Offset)},\"height\":{F(p.Height)},\"startY\":{F(p.StartY)}"
                    + $",\"attenuation\":{F(p.Attenuation)},\"autoExposureLimit\":{F(p.AutoExposureLimit)}"
                    + $",\"legacyAutoExposure\":{(p.LegacyAutoExposure ? "true" : "false")}}}";
            }
        }
    }

    private IEnumerable<string> UnityLights()
    {
        foreach (var l in FindObjectsByType<Light>(FindObjectsSortMode.None))
        {
            var lc = l.GetComponentInParent<LightController>();
            var t = l.transform;
            yield return "{\"name\":\"" + Esc(l.name) + "\",\"type\":\"" + l.type + "\",\"color\":" + C(l.color)
                + ",\"intensity\":" + F(l.intensity) + ",\"range\":" + F(l.range) + ",\"spot\":" + F(l.spotAngle)
                + ",\"enabled\":" + (l.isActiveAndEnabled ? "true" : "false")
                + ",\"pos\":" + V4(t.position * Unit) + ",\"fwd\":" + V4(t.forward)
                + ",\"light\":" + (Driven(lc) ? $"[{lc.Type},{KeysJson(lc)}]" : "null") + "}";
        }
    }

    // Mesh in its own space: positions, normals, uvs and triangle indices. Unreadable meshes are read back
    // from their GPU buffers.
    private static string MeshJson(Renderer r, out bool readable)
    {
        readable = true;
        var verts = new List<Vector3>();
        var norms = new List<Vector3>();
        var uvs = new List<Vector2>();
        var cols = new List<Color>();
        var idx = new List<int>();
        string name;
        switch (r)
        {
            case SpriteRenderer sr when sr.sprite != null:
                name = sr.sprite.name;
                verts.AddRange(sr.sprite.vertices.Select(v => (Vector3)v));
                uvs.AddRange(sr.sprite.uv);
                idx.AddRange(sr.sprite.triangles.Select(i => (int)i));
                break;
            case MeshRenderer or SkinnedMeshRenderer:
                var mesh = r is SkinnedMeshRenderer smr ? smr.sharedMesh : r.GetComponent<MeshFilter>()?.sharedMesh;
                if (mesh == null) return null;
                return MeshJsonOf(mesh, out readable);
            default:
                return null;
        }
        return MeshText(name, verts, norms, uvs, cols, idx);
    }

    private static string MeshJsonOf(Mesh mesh, out bool readable)
    {
        readable = true;
        var verts = new List<Vector3>();
        var norms = new List<Vector3>();
        var uvs = new List<Vector2>();
        var cols = new List<Color>();
        var idx = new List<int>();
        var name = mesh.name;
        switch (0)
        {
            default:
                if (mesh.isReadable)
                {
                    mesh.GetVertices(verts);
                    mesh.GetNormals(norms);
                    mesh.GetUVs(0, uvs);
                    mesh.GetColors(cols);
                    for (var s = 0; s < mesh.subMeshCount; s++)
                        AddFaces(idx, mesh.GetIndices(s), mesh.GetTopology(s));
                }
                else
                {
                    readable = false;
                    if (!GpuRead(mesh, verts, norms, uvs, cols, idx)) return null;
                }
                break;
        }
        return MeshText(name, verts, norms, uvs, cols, idx);
    }

    private static string MeshText(string name, List<Vector3> verts, List<Vector3> norms, List<Vector2> uvs, List<Color> cols, List<int> idx)
    {
        var sb = new StringBuilder();
        sb.Append("{\"name\":\"").Append(Esc(name)).Append("\",\"v\":[");
        sb.Append(string.Join(",", verts.SelectMany(v => new[] { v.x, v.y, v.z }).Select(F)));
        sb.Append("],\"n\":[");
        if (norms.Count == verts.Count) sb.Append(string.Join(",", norms.SelectMany(v => new[] { v.x, v.y, v.z }).Select(F)));
        sb.Append("],\"uv\":[");
        if (uvs.Count == verts.Count) sb.Append(string.Join(",", uvs.SelectMany(v => new[] { v.x, v.y }).Select(F)));
        sb.Append("],\"c\":[");
        if (cols.Count == verts.Count) sb.Append(string.Join(",", cols.SelectMany(c => new[] { c.r, c.g, c.b, c.a }).Select(F)));
        sb.Append("],\"i\":[").Append(string.Join(",", idx.Where(i => i >= 0 && i < verts.Count))).Append("]}");
        return sb.ToString();
    }

    // Triangles as they are; quads split into two triangles.
    private static void AddFaces(List<int> idx, int[] ids, MeshTopology topology)
    {
        if (topology == MeshTopology.Triangles) idx.AddRange(ids);
        else if (topology == MeshTopology.Quads)
            for (var i = 0; i + 3 < ids.Length; i += 4)
                idx.AddRange(new[] { ids[i], ids[i + 1], ids[i + 2], ids[i], ids[i + 2], ids[i + 3] });
    }

    private static float ReadComponent(byte[] b, int at, UnityEngine.Rendering.VertexAttributeFormat f)
    {
        switch (f)
        {
            case UnityEngine.Rendering.VertexAttributeFormat.Float32: return System.BitConverter.ToSingle(b, at);
            case UnityEngine.Rendering.VertexAttributeFormat.Float16: return Mathf.HalfToFloat(System.BitConverter.ToUInt16(b, at));
            case UnityEngine.Rendering.VertexAttributeFormat.SNorm16: return Mathf.Max(System.BitConverter.ToInt16(b, at) / 32767f, -1f);
            case UnityEngine.Rendering.VertexAttributeFormat.UNorm16: return System.BitConverter.ToUInt16(b, at) / 65535f;
            case UnityEngine.Rendering.VertexAttributeFormat.SNorm8: return Mathf.Max((sbyte)b[at] / 127f, -1f);
            case UnityEngine.Rendering.VertexAttributeFormat.UNorm8: return b[at] / 255f;
            default: return 0f;
        }
    }

    private static int ComponentSize(UnityEngine.Rendering.VertexAttributeFormat f) => f switch
    {
        UnityEngine.Rendering.VertexAttributeFormat.Float32 => 4,
        UnityEngine.Rendering.VertexAttributeFormat.Float16 or UnityEngine.Rendering.VertexAttributeFormat.SNorm16
            or UnityEngine.Rendering.VertexAttributeFormat.UNorm16 => 2,
        _ => 1,
    };

    private static bool GpuRead(Mesh mesh, List<Vector3> verts, List<Vector3> norms, List<Vector2> uvs, List<Color> cols, List<int> idx)
    {
        var n = mesh.vertexCount;
        var streams = new Dictionary<int, byte[]>();
        byte[] Stream(int s)
        {
            if (streams.TryGetValue(s, out var b)) return b;
            using var vb = mesh.GetVertexBuffer(s);
            if (vb == null) return streams[s] = null;
            b = new byte[vb.count * vb.stride];
            vb.GetData(b);
            return streams[s] = b;
        }
        List<float[]> Attr(UnityEngine.Rendering.VertexAttribute a, int dims)
        {
            if (!mesh.HasVertexAttribute(a)) return null;
            var s = mesh.GetVertexAttributeStream(a);
            var b = Stream(s);
            if (b == null) return null;
            var off = mesh.GetVertexAttributeOffset(a);
            var fmt = mesh.GetVertexAttributeFormat(a);
            var stride = mesh.GetVertexBufferStride(s);
            var size = ComponentSize(fmt);
            var res = new List<float[]>(n);
            for (var i = 0; i < n; i++)
            {
                var v = new float[dims];
                for (var k = 0; k < dims; k++) v[k] = ReadComponent(b, i * stride + off + k * size, fmt);
                res.Add(v);
            }
            return res;
        }
        try
        {
            var pos = Attr(UnityEngine.Rendering.VertexAttribute.Position, 3);
            if (pos == null) return false;
            verts.AddRange(pos.Select(v => new Vector3(v[0], v[1], v[2])));
            var nor = Attr(UnityEngine.Rendering.VertexAttribute.Normal, 3);
            if (nor != null) norms.AddRange(nor.Select(v => new Vector3(v[0], v[1], v[2])));
            var uv = Attr(UnityEngine.Rendering.VertexAttribute.TexCoord0, 2);
            if (uv != null) uvs.AddRange(uv.Select(v => new Vector2(v[0], v[1])));
            var col = Attr(UnityEngine.Rendering.VertexAttribute.Color, 4);
            if (col != null) cols.AddRange(col.Select(v => new Color(v[0], v[1], v[2], v[3])));
            using var ib = mesh.GetIndexBuffer();
            var raw = new byte[ib.count * ib.stride];
            ib.GetData(raw);
            for (var s = 0; s < mesh.subMeshCount; s++)
            {
                var sm = mesh.GetSubMesh(s);
                var ids = new int[sm.indexCount];
                for (var i = 0; i < sm.indexCount; i++)
                {
                    var at = (sm.indexStart + i) * ib.stride;
                    var v = ib.stride == 2 ? System.BitConverter.ToUInt16(raw, at) : (int)System.BitConverter.ToUInt32(raw, at);
                    ids[i] = v + sm.baseVertex;
                }
                AddFaces(idx, ids, sm.topology);
            }
            return true;
        }
        catch (System.Exception e)
        {
            Debug.LogWarning($"BslProbe: cannot read {mesh.name}: {e.Message}");
            return false;
        }
    }
}
