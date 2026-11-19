"""Release ordering/integrity tests using local Git and a fake gh executable."""

import hashlib
import io
import base64
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import zipfile


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "publish-release.sh"
FAKE_GH = r'''#!/usr/bin/env python3
import base64, hashlib, json, os, pathlib, sys
from urllib.parse import urlsplit, parse_qs
path = pathlib.Path(os.environ['FIXTURE_GH_STATE'])
state = json.loads(path.read_text())
args = sys.argv[1:]
state['commands'].append(args)
def save(): path.write_text(json.dumps(state))
def respond(value):
    save(); print(json.dumps(value)); sys.exit(0)
def fail(message, status):
    save(); print('gh: '+message+' (HTTP '+str(status)+')', file=sys.stderr); sys.exit(1)
if args[0] == 'api':
    endpoint = args[1]
    if endpoint.endswith('/releases') and '--method' in args and args[args.index('--method')+1]=='POST':
        body=json.loads(pathlib.Path(args[args.index('--input')+1]).read_text())
        if body.get('draft') is not True or body.get('prerelease') is not False or 'make_latest' in body:
            fail('Invalid typed draft creation body',422)
        if state.get('release') is not None: fail('Duplicate draft creation',422)
        state.setdefault('create_payloads',[]).append(body)
        state['release']={
          'id':123,'tag_name':body['tag_name'],'name':body['name'],
          'target_commitish':state.get('target_override',body['target_commitish']),
          'draft':True,'prerelease':False,'assets':[],
          'html_url':'https://github.com/acme/flashtool/releases/tag/'+body['tag_name']
        }
        respond(state['release'])
    if '/releases/123/assets?' in endpoint and '--method' in args and args[args.index('--method')+1]=='POST':
        name=parse_qs(urlsplit(endpoint).query)['name'][0]
        if state.get('fail_upload') == name: fail('Upload failed',500)
        if any(a['name']==name for a in state['release']['assets']): fail('Duplicate asset',422)
        data=pathlib.Path(args[args.index('--input')+1]).read_bytes()
        if 'Content-Type: application/octet-stream' not in args or 'Content-Length: '+str(len(data)) not in args: fail('Missing upload headers',422)
        sha=hashlib.sha256(data).hexdigest()
        asset={'id':200+len(state['release']['assets']),'name':name,'size':len(data)}
        if not state.get('no_digest'): asset['digest']='sha256:'+('0'*64 if state.get('corrupt_digest')==name else sha)
        state['release']['assets'].append(asset)
        state.setdefault('payloads',{})[name]=base64.b64encode(data).decode()
        respond(asset)
    if '/releases/assets/' in endpoint:
        if 'Accept: application/octet-stream' not in args: fail('Missing binary accept',422)
        asset_id=int(endpoint.rsplit('/',1)[1])
        asset=next(a for a in state['release']['assets'] if a['id']==asset_id)
        save(); sys.stdout.buffer.write(base64.b64decode(state['payloads'][asset['name']])); sys.exit(0)
    if endpoint.endswith('/releases/123') and '--method' in args and args[args.index('--method')+1]=='PATCH':
        body=json.loads(pathlib.Path(args[args.index('--input')+1]).read_text())
        state.setdefault('patch_payloads',[]).append(body)
        if 'draft' in body:
            if body['draft'] is not False or len(state['release']['assets'])!=3: fail('Publishing before all assets exist',422)
            state['release']['draft']=False
        if body.get('make_latest') not in ('true','false'): fail('Latest must be a string',422)
        if body['make_latest']=='true': state['latest_release']=json.loads(json.dumps(state['release']))
        respond(state['release'])
    if '/git/ref/tags/' in endpoint:
        respond({'object': {'type':'tag','sha':'a'*40}})
    if '/git/tags/' in endpoint:
        respond({'object': {'type':'commit','sha':state.get('remote_commit',state['commit'])}})
    if '/releases/tags/' in endpoint:
        if state.get('deny_release_api'): fail(state.get('denial_message','Forbidden'),403)
        if state.get('release') is None or state['release']['draft']: fail('Not Found',404)
        respond(state['release'])
    if '/releases?per_page=100&page=' in endpoint:
        if state.get('hide_fresh_draft') and state.get('release') is not None and state['release']['draft']:
            respond([])
        page=int(endpoint.rsplit('=',1)[1])
        target_page=state.get('draft_page',1)
        if page<target_page:
            respond([{'id':1000+i,'tag_name':'v9.0.'+str(i),'draft':False} for i in range(100)])
        respond([state['release']] if state.get('release') is not None else [])
    if endpoint.endswith('/releases/123'):
        if state.get('release') is None: fail('Not Found',404)
        respond(state['release'])
    if endpoint.endswith('/releases/latest'):
        if state.get('latest_release') is not None: respond(state['latest_release'])
        if state.get('release') is None or state['release']['draft']: fail('Not Found',404)
        respond(state['release'])
fail('Unexpected mock command',400)
'''

def creates_release(command):
    return command[0]=="api" and command[1].endswith("/releases") and "POST" in command


def mutates_release(command):
    return command[0]=="api" and "--method" in command and command[command.index("--method")+1] in ("POST","PATCH","PUT","DELETE")


def uploads_asset(command):
    return command[0]=="api" and command[1].startswith("https://uploads.github.com/") and "POST" in command


def patches_release(command):
    return command[0]=="api" and "PATCH" in command


def uploaded_name(command):
    from urllib.parse import urlsplit, parse_qs
    return parse_qs(urlsplit(command[1]).query)["name"][0]


class PublishReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        base = Path(self.temporary.name)
        self.root = base / "project"
        self.root.mkdir()
        self.git("init", "--quiet")
        self.git("config", "user.name", "Release Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("remote", "add", "origin", "https://github.com/acme/flashtool.git")
        (self.root / "Cargo.toml").write_text('[package]\nname="flashtool"\nversion="0.3.0"\n')
        (self.root / "CHANGELOG.md").write_text('# Changelog\n\n## [0.3.0]\nNew update checks.\n\n## [0.2.0]\nOld.\n')
        (self.root / "README.md").write_text("Fixture source.\n")
        self.git("add", "Cargo.toml", "CHANGELOG.md", "README.md")
        self.git("commit", "--quiet", "-m", "fixture")
        self.git("tag", "-a", "v0.3.0", "-m", "fixture release")
        self.commit = self.git("rev-parse", "HEAD").strip()
        self.dist = self.root / "dist"
        self.dist.mkdir()
        self.windows_name = "FlashTool-v0.3.0-windows-x64.zip"
        with zipfile.ZipFile(self.dist / self.windows_name, "w") as archive:
            archive.writestr("FlashTool.exe", b"MZfixture PE, never executed")
            archive.writestr("BUILD-METADATA.json", json.dumps({
                "version":"0.3.0", "commit":self.commit, "target":"x86_64-pc-windows-gnu",
                "driver_packages":{"ch343":"not_embedded","cp210x":"not_embedded"},
            }))
        sha = hashlib.sha256((self.dist / self.windows_name).read_bytes()).hexdigest()
        (self.dist / "SHA256SUMS-v0.3.0").write_text(f"{sha}  {self.windows_name}\n")
        bin_directory = base / "bin"
        bin_directory.mkdir()
        command = bin_directory / "gh"
        command.write_text(FAKE_GH)
        command.chmod(0o755)
        self.state_path = base / "gh-state.json"
        self.save({"commit":self.commit,"release":None,"commands":[]})
        self.environment = {
            **os.environ, "PATH":str(bin_directory)+os.pathsep+os.environ["PATH"],
            "FIXTURE_GH_STATE":str(self.state_path), "GH_REPO":"acme/flashtool",
        }

    def git(self, *arguments):
        return subprocess.run(["git",*arguments],cwd=self.root,capture_output=True,text=True,check=True).stdout

    def save(self, state):
        self.state_path.write_text(json.dumps(state))

    def load(self):
        return json.loads(self.state_path.read_text())

    def invoke(self, *arguments):
        return subprocess.run(
            ["bash",str(SCRIPT),"--root",str(self.root),*arguments],
            env=self.environment,capture_output=True,text=True,timeout=30,
        )

    def prepare_source_version(self, version):
        files={
            "Cargo.toml":f'[package]\nname="flashtool"\nversion="{version}"\n',
            "Cargo.lock":"version=4\n", "rust-toolchain.toml":'[toolchain]\nchannel="stable"\n',
            "build.rs":"fn main() {}\n", "README.md":"Build instructions.\n",
            "CHANGELOG.md":f"# Changelog\n\n## [{version}]\nImmutable source test.\n",
            ".gitattributes":"*.bin -text\n", ".gitignore":"__pycache__/\n",
            "src/lib.rs":"pub fn fixture() {}\n", "assets/drivers/cp210x.zip":"fixture driver ZIP bytes",
            "assets/drivers/ch343.exe":"fixture original driver",
            "scripts/build.ps1":"Write-Host fixture\n", "tests/source.rs":"// fixture test\n",
            "docs/log-upload-api.md":"# Device log upload API\n",
            "examples/self_update_smoke.rs":"// Isolated update helper smoke fixture.\n",
            ".github/workflows/windows.yml":"name: fixture\n", "LICENSE-MIT":"Fixture licence.\n",
            "Package-Tool-Setup-0.2.220.exe":"legacy unrelated installer"*1000,
            "attachments/private.bin":"private input", "dist/private.log":"private build log",
        }
        for name,value in files.items():
            path=self.root/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_text(value)
        self.git("add","--",*files.keys())
        self.git("commit","--quiet","-m","source fixture "+version)
        self.git("tag","-a","v"+version,"-m","version fixture")
        self.commit=self.git("rev-parse","HEAD").strip()
        self.windows_name=f"FlashTool-v{version}-windows-x64.zip"
        with zipfile.ZipFile(self.dist/self.windows_name,"w") as archive:
            archive.writestr("FlashTool.exe",b"MZfixture PE, never executed")
            archive.writestr("BUILD-METADATA.json",json.dumps({"version":version,"commit":self.commit,"target":"x86_64-pc-windows-msvc"}))
        sha=hashlib.sha256((self.dist/self.windows_name).read_bytes()).hexdigest()
        (self.dist/("SHA256SUMS-v"+version)).write_text(f"{sha}  {self.windows_name}\n")
        self.save({"commit":self.commit,"release":None,"commands":[]})
        return files

    def test_new_source_archive_contains_build_inputs_but_not_legacy_or_private_files(self):
        files=self.prepare_source_version("0.4.2")
        result=self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        raw=base64.b64decode(self.load()["payloads"]["FlashTool-v0.4.2-source.zip"])
        with zipfile.ZipFile(io.BytesIO(raw)) as archive:
            names=set(archive.namelist());prefix="FlashTool-v0.4.2/"
            excluded={"Package-Tool-Setup-0.2.220.exe","attachments/private.bin","dist/private.log","docs/log-upload-api.md","examples/self_update_smoke.rs"}
            for name in files:
                if name in excluded:self.assertNotIn(prefix+name,names)
                else:self.assertEqual(archive.read(prefix+name),files[name].encode())
            self.assertFalse(any("attachments/" in name or "/dist/" in name for name in names))

    def test_050_source_archive_contains_api_documentation(self):
        files=self.prepare_source_version("0.5.0")
        result=self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        raw=base64.b64decode(self.load()["payloads"]["FlashTool-v0.5.0-source.zip"])
        with zipfile.ZipFile(io.BytesIO(raw)) as archive:
            self.assertEqual(archive.read("FlashTool-v0.5.0/docs/log-upload-api.md"),files["docs/log-upload-api.md"].encode())
            self.assertNotIn("FlashTool-v0.5.0/Package-Tool-Setup-0.2.220.exe",archive.namelist())
            self.assertNotIn("FlashTool-v0.5.0/examples/self_update_smoke.rs",archive.namelist())

    def test_051_source_archive_contains_update_smoke_build_inputs(self):
        files=self.prepare_source_version("0.5.1")
        result=self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        raw=base64.b64decode(self.load()["payloads"]["FlashTool-v0.5.1-source.zip"])
        with zipfile.ZipFile(io.BytesIO(raw)) as archive:
            self.assertEqual(archive.read("FlashTool-v0.5.1/examples/self_update_smoke.rs"),files["examples/self_update_smoke.rs"].encode())
            self.assertIn("FlashTool-v0.5.1/docs/log-upload-api.md",archive.namelist())
            self.assertNotIn("FlashTool-v0.5.1/Package-Tool-Setup-0.2.220.exe",archive.namelist())

    def test_043_source_archive_keeps_its_existing_path_policy(self):
        self.prepare_source_version("0.4.3")
        result=self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        raw=base64.b64decode(self.load()["payloads"]["FlashTool-v0.4.3-source.zip"])
        with zipfile.ZipFile(io.BytesIO(raw)) as archive:
            self.assertNotIn("FlashTool-v0.4.3/docs/log-upload-api.md",archive.namelist())
            self.assertIn("FlashTool-v0.4.3/README.md",archive.namelist())
            self.assertNotIn("FlashTool-v0.4.3/Package-Tool-Setup-0.2.220.exe",archive.namelist())

    def test_legacy_041_source_archive_remains_byte_identical(self):
        self.prepare_source_version("0.4.1")
        expected=subprocess.run(["git","archive","--format=zip","--prefix","FlashTool-v0.4.1/",self.commit],cwd=self.root,capture_output=True,check=True).stdout
        result=self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        actual=base64.b64decode(self.load()["payloads"]["FlashTool-v0.4.1-source.zip"])
        self.assertEqual(actual,expected)
        with zipfile.ZipFile(io.BytesIO(actual)) as archive:
            self.assertIn("FlashTool-v0.4.1/Package-Tool-Setup-0.2.220.exe",archive.namelist())
        previous=self.load()
        result=self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual([c for c in self.load()["commands"] if mutates_release(c)],[c for c in previous["commands"] if mutates_release(c)])

    def test_offline_plan_has_exact_tag_commit_and_three_assets(self):
        before = {p.name:p.read_bytes() for p in self.dist.iterdir()}
        result = self.invoke("--check", "--dry-run")
        self.assertEqual(result.returncode,0,result.stderr)
        plan = json.loads(result.stdout)
        self.assertEqual((plan["tag"],plan["title"],plan["commit"]),("v0.3.0","FlashTool v0.3.0",self.commit))
        self.assertEqual(set(plan["assets"]),{self.windows_name,"FlashTool-v0.3.0-source.zip","SHA256SUMS"})
        self.assertEqual(self.load()["commands"],[])
        self.assertEqual(before,{p.name:p.read_bytes() for p in self.dist.iterdir()})

    def test_draft_assets_are_verified_before_publish_and_second_run_is_immutable(self):
        result = self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        state=self.load()
        self.assertFalse(state["release"]["draft"])
        commands=state["commands"]
        create=next(c for c in commands if creates_release(c))
        self.assertIn("--input",create)
        self.assertIs(state["create_payloads"][0]["draft"],True)
        self.assertIs(state["create_payloads"][0]["prerelease"],False)
        self.assertNotIn("make_latest",state["create_payloads"][0])
        uploads=[i for i,c in enumerate(commands) if uploads_asset(c)]
        publish=next(i for i,c in enumerate(commands) if patches_release(c))
        self.assertEqual(len(uploads),3)
        self.assertLess(max(uploads),publish)
        self.assertTrue(any(c[0]=="api" and c[1].endswith("/releases/123") for c in commands[max(uploads)+1:publish]))
        mutations=[c for c in commands if mutates_release(c)]
        result=self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual([c for c in self.load()["commands"] if mutates_release(c)],mutations)

    def test_missing_api_digests_fall_back_to_download_and_hash(self):
        state=self.load();state["no_digest"]=True;self.save(state)
        result=self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(sum(c[0]=="api" and "/releases/assets/" in c[1] for c in self.load()["commands"]),3)

    def test_resumes_an_existing_paginated_draft_without_duplicate_creation(self):
        state=self.load();state["fail_upload"]="FlashTool-v0.3.0-source.zip";self.save(state)
        result=self.invoke()
        self.assertNotEqual(result.returncode,0)
        state=self.load()
        self.assertTrue(state["release"]["draft"])
        self.assertEqual([a["name"] for a in state["release"]["assets"]],[self.windows_name])
        del state["fail_upload"]
        state["draft_page"]=2
        self.save(state)
        result=self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        state=self.load()
        self.assertFalse(state["release"]["draft"])
        self.assertEqual(sum(creates_release(c) for c in state["commands"]),1)
        self.assertTrue(any(c[0]=="api" and c[1].endswith("page=2") for c in state["commands"]))
        self.assertEqual(sum(uploads_asset(c) and uploaded_name(c)==self.windows_name for c in state["commands"]),1)

    def test_fresh_post_response_id_works_before_draft_indexes_are_updated(self):
        state=self.load();state["hide_fresh_draft"]=True;self.save(state)
        result=self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        state=self.load()
        self.assertFalse(state["release"]["draft"])
        self.assertEqual(sum(creates_release(c) for c in state["commands"]),1)
        create_index=next(i for i,c in enumerate(state["commands"]) if creates_release(c))
        self.assertFalse(any(c[0]=="api" and ("/releases/tags/" in c[1] or "/releases?" in c[1]) for c in state["commands"][create_index+1:]))

    def test_backfilling_an_older_release_does_not_downgrade_latest(self):
        state=self.load()
        state["latest_release"]={"id":400,"tag_name":"v0.4.0","draft":False,"prerelease":False}
        self.save(state)
        result=self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        state=self.load()
        self.assertFalse(state["release"]["draft"])
        self.assertEqual(state["latest_release"]["tag_name"],"v0.4.0")
        publish=next(c for c in state["commands"] if patches_release(c))
        self.assertEqual(state["patch_payloads"][0],{"draft":False,"make_latest":"false"})
        mutations=[c for c in state["commands"] if mutates_release(c)]
        result=self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(self.load()["latest_release"]["tag_name"],"v0.4.0")
        self.assertEqual([c for c in self.load()["commands"] if mutates_release(c)],mutations)

    def test_newer_stable_release_advances_latest(self):
        state=self.load()
        state["latest_release"]={"id":200,"tag_name":"v0.2.0","draft":False,"prerelease":False}
        self.save(state)
        result=self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        state=self.load()
        self.assertEqual(state["latest_release"]["tag_name"],"v0.3.0")
        self.assertEqual(state["latest_release"]["id"],state["release"]["id"])
        publish=next(c for c in state["commands"] if patches_release(c))
        self.assertEqual(state["patch_payloads"][0],{"draft":False,"make_latest":"true"})

    def test_branch_target_metadata_uses_the_exact_remote_tag_commit(self):
        state=self.load();state["target_override"]="main";self.save(state)
        result=self.invoke()
        self.assertEqual(result.returncode,0,result.stderr)
        state=self.load()
        self.assertEqual(state["release"]["target_commitish"],"main")
        self.assertFalse(state["release"]["draft"])
        tag_reads=[c for c in state["commands"] if c[0]=="api" and "/git/ref/tags/" in c[1]]
        self.assertGreaterEqual(len(tag_reads),2)

    def test_wrong_explicit_target_sha_is_rejected(self):
        for target in ("b"*40,"b"*64):
            with self.subTest(target=target):
                self.save({"commit":self.commit,"release":None,"commands":[],"target_override":target})
                result=self.invoke()
                self.assertNotEqual(result.returncode,0)
                self.assertIn("different explicit target SHA",result.stderr)
                self.assertTrue(self.load()["release"]["draft"])
                self.assertFalse(any(patches_release(c) for c in self.load()["commands"]))

    def test_http_403_is_not_treated_as_release_absence(self):
        state=self.load();state["deny_release_api"]=True;self.save(state)
        result=self.invoke()
        self.assertNotEqual(result.returncode,0)
        self.assertIn("HTTP 403",result.stderr)
        self.assertFalse(any(mutates_release(c) for c in self.load()["commands"]))

    def test_github_actions_failure_annotation_is_escaped_and_sanitized(self):
        self.environment["GITHUB_ACTIONS"]="true"
        state=self.load()
        state["deny_release_api"]=True
        state["denial_message"]="\x1b[31mForbidden 100% token=ghp_FAKE_TEST_ONLY\npermission denied"
        self.save(state)
        result=self.invoke()
        self.assertEqual(result.returncode,1)
        annotation=next(line for line in result.stdout.splitlines() if line.startswith("::error"))
        self.assertIn("api repos/acme/flashtool/releases/tags/v0.3.0",annotation)
        self.assertIn("HTTP 403",annotation)
        self.assertIn("100%25",annotation)
        self.assertIn("%0A",annotation)
        self.assertNotIn("ghp_FAKE_TEST_ONLY",annotation)
        self.assertNotIn("\x1b",annotation)
        self.assertIn("Release failed:",result.stderr)
        self.assertNotIn("ghp_FAKE_TEST_ONLY",result.stderr)
        self.assertFalse(any(mutates_release(c) for c in self.load()["commands"]))

    def test_upload_failure_or_checksum_failure_never_publishes(self):
        for setting in ("fail_upload","corrupt_digest"):
            with self.subTest(setting=setting):
                self.save({"commit":self.commit,"release":None,"commands":[],setting:self.windows_name})
                result=self.invoke()
                self.assertNotEqual(result.returncode,0)
                state=self.load()
                self.assertTrue(state["release"]["draft"])
                self.assertFalse(any(patches_release(c) for c in state["commands"]))

    def test_checksum_tag_and_remote_commit_mismatch_fail(self):
        result=self.invoke("--check","--tag","v0.2.0")
        self.assertNotEqual(result.returncode,0)
        state=self.load();state["remote_commit"]="b"*40;self.save(state)
        result=self.invoke()
        self.assertNotEqual(result.returncode,0)
        self.assertFalse(any(mutates_release(c) for c in self.load()["commands"]))
        (self.dist/self.windows_name).write_bytes(b"changed after build")
        result=self.invoke("--check")
        self.assertNotEqual(result.returncode,0)
        self.assertIn("checksum",result.stderr)


if __name__ == "__main__":
    unittest.main()
